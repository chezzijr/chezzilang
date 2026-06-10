# Chezzi — B3: Tier-C Shared-Nothing OS-Thread Multicore (phased, multi-session plan)

> **Status:** design + phased implementation plan — **engine code not started**. This is the
> persistent, multi-session source of truth for **B3**. A future session picks the lowest unfinished
> phase, implements it TDD, ships it, and ticks it here. The high-level A/B roadmap lives in
> [`concurrency.md` §9](concurrency.md#9-implementation-roadmap-c1c5); this file is the *execution
> plan* for the B3 epic specifically.
>
> Companion context you should read first: `concurrency.md` (§2 the tier table, §4 staged executor,
> §5–§7 Channel/Shared/sendability) and `concurrency.md` §9's "B1 + B2 as shipped on the VM".
>
> **All line references are anchors against `master` at authoring time; re-grep by symbol name if
> they have drifted.**

---

## 1. Goal & the load-bearing invariant

Make `parallel:` / `spawn` / `Channel[T]` / `Shared[T]` / `Executor` run on **real OS threads** —
true multicore — with the **surface completely unchanged**. This is Tier C from
[`concurrency.md` §2](concurrency.md#2-why-shared-nothing-and-why-not-the-go-memory-model):
shared-nothing threads + channels, **no `Rc`→`Arc` on `Value`, no concurrent GC**.

**The invariant that makes it cheap:** each worker thread owns its **own heap + its own mark-sweep
GC**. No value in one thread's heap ever references another's. A `Value::Obj(GcRef)` is a slot index
into *one specific* heap (`src/vm/value.rs:13` — `GcRef(pub u32)`), meaningless anywhere else — so
GcRefs *cannot* cross threads, and per-thread collection needs **no stop-the-world, no handshake, no
atomic mark bits**. Everything below exists to preserve this: values cross the thread boundary only
as a **serialized, `Send` wire form**, never as live heap references.

The interpreter (`src/interp/`) is **not** part of B3 — it stays the frozen sequential parity oracle
(suspendable/parallel interp is a permanent non-goal, see `concurrency.md` §9). **The VM is the sole
concurrent engine.**

---

## 2. Architecture (validated against source)

1. **Per-thread worker `Vm` + own heap/GC.** `Vm::new` already takes the program by `Rc<Program>`
   (`src/vm/mod.rs:290`). The compiled program (protos, bytecode, constants, function table) is
   **immutable after compile**, so `Rc<Program>` → `Arc<Program>` and every worker VM shares it
   read-only. A worker = `Arc<Program>` + a fresh `Heap` + its own `frames`/`stack`/`out`/`stderr`.

2. **Wire-format airlock — replaces `deep_clone`.** A `WireValue` enum mirrors the **sendable** value
   set (`concurrency.md` §7): `Int/Float/Bool/Str/Nil`, `List/Map/Set/Tuple`, `Struct`, `Enum`, plus
   **`Channel`/`Shared` handles** (carried as the `Arc<…Core>` itself — see #3/#4). Two functions:
   - `to_wire(&self, v: Value) -> WireValue` — the same tree-walk `deep_clone` does today
     (`src/vm/mod.rs:2913`), but emitting a `Send` value instead of fresh heap objects.
   - `from_wire(&mut self, w: WireValue) -> Value` — reconstructs the value in the *destination*
     heap.
   Only sendable values reach `to_wire` (checker-gated at `spawn`/`send`/`submit`); a non-sendable
   value is a **defensive runtime fault**, *not* `unreachable!()` (see §3 decision-E gap and the
   `Shared.update` return-value path).

3. **`Channel` core moves OUT of the heap.** Today `Obj::Channel(VecDeque<Value>)` lives in the heap
   and its queue is GC-traced (`src/vm/heap.rs:273`). A heap-resident `VecDeque<Value>` holds GcRefs
   into *that* heap → cannot be shared. Under B3:
   - The shared object lives outside every heap: `Arc<ChannelCore>` where
     `ChannelCore { q: Mutex<VecDeque<WireValue>>, cv: Condvar, /* + close/cancel bits later */ }`.
   - The heap holds only a **handle** (`Obj::Channel(Arc<ChannelCore>)`). The `children()` arm is
     **rewritten** to trace `Handle(GcRef)`s embedded in the core's `WireValue`s (B3.1 — see decision
     E), and only **dropped** once `WireValue` is fully `GcRef`-free at B3.3. (B3.1 ships
     `ChannelCore { q: Mutex<VecDeque<WireValue>> }`; the `cv`/close/cancel bits land at B3.3/B3.4.)
   - `send` = lock + `push_back(to_wire(v))`; `recv` = lock + `pop_front` + `from_wire` (B3.3 adds the
     `cv.wait` real-OS-thread blocking path); `try_recv` = lock + pop-or-`None`; `len` = lock + len.

4. **`Shared` core moves OUT of the heap** (this **subsumes B4**). `Arc<SharedCore { Mutex<WireValue> }>`;
   the heap holds the handle, `children()` `Shared` arm rewritten (B3.1; dropped at B3.3). `get`/`set` lock + wire-convert;
   `update` locks, `from_wire`s the value into the **calling** thread's heap, runs the user closure
   there, then `to_wire`s the result back under the lock.

5. **`parallel:` runs task bodies on a bounded pool** (decision B); `JoinNursery` joins all handles;
   the first child fault aborts siblings via cooperative cancel (decision C) and propagates, composing
   with `recover:` / `defer`. `spawn:` already compiles to a synthetic zero-arg closure proto and
   `spawn f(x)` is a proto + args — both ship the proto by `Arc` (read-only) and captures/args by
   wire.

6. **`Executor` (B5) rides the same pool**; **A3b** (submit-capture sendability gate) becomes
   load-bearing here, because the submitted closure's captures now truly cross a thread.

---

## 3. Decisions (rationale recorded so future sessions don't relitigate)

### A. Determinism contract — **the highest-priority decision**

Real parallelism makes output ordering **nondeterministic**, which breaks *every* byte-identical
golden — most sharply the cooperative interleave goldens (e.g. `channel_block.expected`, ping-pong's
exact `pong 0 / ping 100 / …`) and the **VM == interp** parity (interp stays sequential forever). You
cannot keep those goldens *and* have real parallelism produce them — the contracts contradict.

**Decision: keep the cooperative single-thread engine as the DEFAULT; gate OS-thread multicore behind
a `--parallel` flag** (mirror the existing `--interp` flag plumbing — `src/main.rs:184`, and note the
"flags must precede the file" gotcha at `src/main.rs:175`). Consequences:

- Every existing concurrency golden + GC-stress variant + the 3-way VM==interp parity **stays green,
  untouched**, on the default engine. The proven regression net is preserved.
- The parallel engine gets its **own** test suite that is **deterministic-by-construction**: tasks
  collect results into a `Channel`, the parent drains after join, **sorts**, prints once — plus
  order-insensitive assertions (compare the *set* of output lines, not their order).
- **VM==interp parity is suspended by definition under `--parallel`.** The interp is the frozen
  sequential oracle and will never be parallel. Documented contract: *in default (cooperative) mode,
  `VM == interp == .expected` holds for the sequential subset; in `--parallel` mode, only
  order-insensitive / collect-sort-print goldens apply, VM-only.* The oracle does not extend to the
  parallel engine.
- This is also what lets phases **B3.0–B3.2 ship behind unchanged behavior** (below).

### B. Bounded work pool, not thread-per-task

`parallel:` with N spawns must **not** be N OS threads: nested `parallel:` (the scheduler is a *stack*
of nurseries; `run_child` supports a nested level) would explode N×M threads. **Decision: a bounded
pool sized to `std::thread::available_parallelism()`.** The thread that hits `JoinNursery` **participates
as a worker** (it is parked anyway today). Known v1 hazard: a bounded pool + blocking tasks can starve
(all pool threads blocked in `recv` on a producer that's queued-but-unscheduled). v1 mitigation:
parent-participates + a documented "tasks should not out-block the pool" rule; work-stealing /
grow-on-stall is deferred.

### C. Cancellation / abort-siblings — cooperative cancel flag, no thread kill

Rust has no safe thread kill. **Decision: per-nursery `cancel: Arc<AtomicBool>`**, checked (Relaxed)
at the **same back-edge / call / channel-op sites the dispatch loop already visits** for `pending_exit`
/ `gc_stress` (`src/vm/mod.rs` around the dispatch top + back-jumps). First fault sets `cancel`;
running siblings observe it at their next check and unwind as a `Cancelled` sentinel the parent
swallows.

- **A condvar-blocked `recv` must also wake on cancel.** A blocked `recv` waits on *both* the
  channel's `cv` **and** a per-nursery cancel condvar (cancel `notify_all`s it); after each wake it
  re-checks `cancel` (the wait is a loop anyway, for spurious wakeups). Getting this wrong = siblings
  hang forever on abort. This is risk #2 (§5).
- **`std.os.exit` in a child** = a fault-that-cancels. `request_exit` (`src/native/os.rs:42`) sets
  the **worker's** `pending_exit` — but `pending_exit` is VM-global today (`src/vm/mod.rs:182`), so
  under B3 each worker has its own. The worker stops, returns its exit code up the join boundary, the
  join sets the nursery `cancel`, and the code propagates to the **parent VM's** `pending_exit`
  (hard halt). It composes with the cancel machinery rather than needing a separate path.

### D. Deadlock detection — keep the nursery-local case, accept global hangs (Go-like)

With real condvar blocking, the automatic "no runnable fiber" detector is gone. **Decision: preserve
the nursery-local all-blocked detector** (it backs the existing golden — `"deadlock: every task in
this parallel: block is blocked…"`, `src/vm/mod.rs:2816`) via a per-nursery counter of
siblings-currently-blocked-in-recv vs live-sibling-count: when `blocked == live` and the channels they
wait on are all empty, no sibling can ever send → broadcast a deadlock fault. **Accept hangs** for
deadlocks spanning nurseries or involving `Executor` (document it, like Go). No global cycle detector
(it would need cross-thread inspection that contradicts shared-nothing). Risk: the counter logic is
race-prone — a false-positive deadlock fault is the failure mode to test against (§5 risk #6).

### E. GC + Arc cores

- **Per-thread GC and the cores.** The end-state goal is that `children()` returns nothing for a core
  (tracing stops at the `Arc` boundary). **But this is only reachable once `WireValue` is fully
  `GcRef`-free**, which it is *not* until B3.3: at B3.1 the cores hold `WireValue`s that can still
  embed `Handle(GcRef)`s — a `Channel[str]` queues `Str` handles, and an `Executor` queues submitted
  **closures** as `Handle(closure)` (closures can't cross by value until the G1 module-globals
  decision lands at B3.3). Those GcRefs point into the live heap and must stay rooted.
  - **B3.1 (landed): the `children()` arms are NOT dropped — they are *rewritten*.** For
    `Obj::Channel/Shared/Executor(Arc<…Core>)` `children()` locks the core and walks its `WireValue`s
    via `WireValue::collect_gcrefs`, yielding the embedded `Handle` GcRefs (it stops at a nested core —
    that core is rooted via its own handle). This is what keeps `Channel[str]`/executor closures alive
    under `gc_stress` (proof: `executor_autodrain_survives_gc_stress`, `executor_tasks_survive_gc_stress`,
    `shared_box_survives_gc_stress`, `spawn_pending_tasks_survive_gc_stress`).
  - **B3.3 (future): drop the arms** once `Str` crosses by value (owned-bytes wire arm) and G1 lets
    closures cross by value — only then do cores genuinely hold no `GcRef`.
- **Known leak — do NOT claim "no cycles".** Reply-channel `Arc` cycles are reachable: `ChannelCore A`'s
  queue holds `WireValue::Channel(Arc<B>)` and B's holds `Arc<A>`; drop both handles and the pair
  leaks for the program's lifetime — `Arc` is refcounted, not cycle-collected. A proper fix needs a
  cross-thread cycle collector, which contradicts shared-nothing. **Documented limitation** (matches
  Go/Rust `Arc` semantics); not a crash, an unbounded leak in a long-running program.

### F. Output — buffer-per-worker, flush-on-join

Each worker VM accumulates its own `out` / `stderr` `String` (`src/vm/mod.rs:166`/`168`, init `:296`/
`:297`). **Decision: each worker returns its `out` on join; the parent concatenates in join order.** A
child's output appears all-at-once at its join point (this *changes* what an interleave golden would
show — fine, because such goldens stay on the cooperative default engine per decision A). This is the
**only** option compatible with keeping any golden green; live shared-stdout interleave is rejected.

### G. Hardest problems, ranked

1. **Mutable module globals across threads — ✅ RESOLVED (Option A).** `do_spawn_block` passes `home`
   (the module-globals object) **by handle** today; under threads that `GcRef` points into the *parent*
   heap and cannot cross, and mutable globals can't be trivially wire-copied (mutations wouldn't
   propagate back). The two candidates were **(A) module globals immutable after init** (a spec
   restriction) vs **(B) per-worker snapshots** (a semantic change where a worker's writes silently
   never propagate back).

   **Decision: Option A.** A module global is just a value at the top scope; mutating it across a task
   is the **same move the checker already bans for `Ref[T]`** at the `spawn` boundary
   (`Ref` is non-sendable — `src/checker/tests.rs` *"non-sendable value of type Ref[int]"*). Chezzi's
   mutation ladder is already **`value` (copy) → `Ref[T]` (in-task box) → `Shared[T]` (cross-task box)**
   (`docs/syntax.md` §11); Option A simply applies the existing top rung to globals. Option B was
   rejected: it makes a write that *looks* global silently local-only — a footgun with no precedent in
   the language, and it contradicts shared-nothing (there is no place for the write to propagate *to*).

   **What Option A means concretely (B3.3 implements):**
   - Under `--parallel`, a module global is **read-only after the module's init prologue runs**: a
     `SetGlobal` (`counter = …` / `counter += …`) reachable from inside a `spawn`'d task (directly or
     transitively) is a **checker error** — *"cannot mutate module global `counter` from a parallel task;
     use Shared[T]"*. Reads of post-init-constant globals are fine.
   - The worker still gets its `home`, but as a **read-only snapshot** of the parent's globals (wire-
     copied in at spawn; never copied back). Safe because nothing writes them.
   - Cross-task mutable state goes through `Shared[T]` (sendable, crosses as the shared `Arc`), exactly
     as the ladder already directs:
     ```chezzi
     counter := Shared(0)                      # not  counter := 0
     fn bump(): counter.update(fn(n): n + 1)
     parallel:
         spawn bump()
         spawn bump()
     print(counter.get())                      # 2
     ```
   - **Default (cooperative, non-`--parallel`) engine is unchanged** — global mutation stays legal there
     (single heap, decision A keeps it the default). The restriction is scoped to `--parallel` task
     bodies, mirroring how blocking-`recv` semantics already differ by engine.

   Open sub-question for the B3.3 session: whether the checker gate is **whole-program** (any global
   reassigned anywhere ⇒ unsendable as a captured read) or **flow-scoped to `spawn` reachability**. Lean
   flow-scoped to avoid over-restricting purely-sequential code in a program that also uses `--parallel`.
2. **Cancellation of a condvar-blocked `recv`** (decision C) — lost-wakeup bugs; naive design hangs
   siblings forever.
3. **Pool sizing vs blocking tasks** (decision B) — bounded pool + blocking recv is a classic
   starvation/deadlock combination.
4. **Output ordering contract** (decision F) — trivial to code, but the *decision* determines whether
   any test contract survives.
5. **Arc-cycle leaks** (decision E) — low correctness risk, real unbounded leak, no clean fix.
6. **Race-free nursery-local deadlock counter** (decision D).

---

## 4. Phased breakdown

Each phase is independently shippable and TDD'd (`cargo test` + `cargo test conformance` + `cargo
clippy` green; update this file + `PROGRESS.md`; commit). **B3.0–B3.2 ship behind unchanged behavior**
(parity + GC-stress goldens exercise them) so the serialization + worker-VM machinery is de-risked
*before a single thread is spawned*. `--parallel` (and any nondeterminism) appears only at **B3.3**.

| Phase | What | TDD focus | Behavior |
|-------|------|-----------|----------|
| **B3.0** | **Wire-format airlock, single-thread.** Define `WireValue`; replace the `deep_clone` call sites (spawn / `Channel.send` / `Shared` get-set: re-grep — today they route through `deep_clone`, `src/vm/mod.rs:2913`) with a `to_wire`+`from_wire` round-trip into the *same* heap. | All existing concurrency goldens + GC-stress stay green (byte-identical); unit test `from_wire(to_wire(v))` structurally equals `deep_clone(v)` over the sendable set; a non-sendable value hits the defensive fault, not `unreachable!`. | **unchanged** |
| **B3.1** ✅ | **Move Channel/Shared/Executor cores out of the heap** into `Arc<…Core>` holding `WireValue`. Cores: `ChannelCore { q: Mutex<VecDeque<WireValue>> }`, `SharedCore { v: Mutex<WireValue> }`, `ExecutorCore { inner: Mutex<ExecState{queue,shut}> }` (`src/vm/core.rs`; no `Condvar` yet — cooperative `recv` parks the fiber, a condvar would be dead until B3.3). `to_wire`/`from_wire` gain `Channel/Shared/Executor(Arc<…Core>)` arms (cross as the shared `Arc`; `from_wire` allocs a fresh handle onto the same core). **`children()` arms REWRITTEN, not dropped** (see decision E — cores still embed `Handle(GcRef)`s single-thread). `pick_runnable` polls `core.q.lock()` length. | Same goldens green incl. `channel_block.expected` and the all-blocked deadlock golden; GC-stress still green (parked fibers + cores survive). | **unchanged** |
| **B3.2** ✅ | **`Arc<Program>` + worker-VM construction (no threads).** `spawn` builds a fresh worker `Vm` sharing `Arc<Program>`, runs the task **synchronously** in it, wire-copies args in and result + `out` back. Resolves the worker-VM/heap-handoff plumbing in isolation. | Goldens green; a unit test proves a task runs in a distinct heap and its result/`out` come back correctly. | **unchanged** |
| **B3.3a** ✅ | **`str` crosses the airlock by value.** Add an owned-bytes `WireValue::Str(Box<str>)` arm; `to_wire`/`from_wire`/`display_wire`/`collect_core_gcrefs` handle it; `str` is no longer a by-reference `Handle`, so `ensure_crossable` lets it (and data containing it) cross a worker boundary. Single-thread, parity-preserved. | `worker_crosses_str_by_value` (was the reject test); `wire_crosses_str_by_value` (fresh handle, value-equal); `wire_str_map_key_survives_roundtrip` (cached hash preserved). All existing concurrency + GC-stress goldens byte-identical. | **unchanged** |
| **B3.3b** ✅ | **G1 module-globals checker gate.** A reassignment (`=`/`+=`/`-=`) of a module global reachable — directly or transitively through free-function calls — from a `spawn` task is a checker error (*"…use Shared[T]"*). Flow-scoped to spawn-reachability; scope-aware name resolution (params/`let`/`for`/`match`/closure/comprehension binders). Direct in-`spawn:`-block writes stay caught by the existing `is_captured` gate. | `spawn_{transitive,deeply_transitive,block_calls,compound_assign,…inside_if,…through_arg_expr,…inside_recover}_*_rejected` + `{sequential,local_shadows,reads,shared_update,callee_shadowed_by_local}_*_ok`. Reviewed by a 4-agent panel + cold pass. | **unchanged** |
| **B3.3c** ✅ | **Read-only `home` snapshot — worker module-graph reconstruction** (single-thread, parity-preserved). `Vm::build_worker_modules` snapshots the parent's initialized `module_objs` into the worker heap (two-pass); `map_global_value` rebuilds `Func`/`Closure`/`Module`/`Native` explicitly and recurses through containers so no nested callable smuggles a parent `GcRef`. A task can now read post-init globals + call sibling/imported fns. | `worker_reads_module_global`, `worker_calls_sibling_free_fn`, `worker_calls_imported_fn`, `worker_calls_through_global_fn_container` (GcRef-smuggle regression), `worker_reconstruction_survives_gc_stress`. Existing goldens byte-identical. | **unchanged** |
| **B3.3d** ✅ | **Method tasks** (`spawn obj.m()`): `run_task_isolated` lowers to `Lowered::Method` (recv + args by wire) and dispatches via `do_method_call` against the reconstructed `module_objs`. A blocking `recv` faults cleanly (no scheduler in a sync worker). | `worker_runs_method_task`, `worker_method_on_struct` (reads a module global through the rebuilt home). | **unchanged** |
| **B3.3-threads** ✅ | **Real OS threads behind `--parallel`.** Bounded pool (decision B), condvar `recv` (decision C blocking), buffer-flush-on-join (decision F). Cooperative engine stays the **default**. The two B3.3 "owes" (read-only `home` snapshot + method tasks) are now **discharged by B3.3c/d** — this phase is purely the thread-flip: the `--parallel` flag + pool + condvar `recv` wire `run_task_isolated` (which is reachable today only from unit tests) onto real threads. | NEW `--parallel`-only goldens that are deterministic-by-construction (collect→drain→sort→print) + order-insensitive (set-of-lines) assertions; every existing golden stays on the default engine and stays green. | **`--parallel` new** |
| **B3.4** ✅ | **Cancellation + cross-thread `os.exit`.** Per-nursery `cancel: Arc<AtomicBool>` (decision C), cross-thread exit-code propagation up the join. Wake-on-cancel uses a **`recv` `wait_timeout` re-checking loop**, not a separate cancel condvar (see decision-C note below). | First-fault-aborts-running-siblings; a child `os.exit` halts the process with the right code; `recover:`/`defer` still compose. | `--parallel` |
| **B3.5** | **Nursery-local deadlock detection under threads** (blocked-count vs live-count, decision D). | Port the all-blocked deadlock golden to `--parallel`; a near-miss (one sibling that *does* send) must NOT false-positive. | `--parallel` |
| **B3.6** | **`Executor` / B5 on the pool + A3b submit-capture sendability gate.** Submitted tasks run on pool threads; the checker now gates `submit`'s closure captures like `spawn` does. | `submit` of a non-sendable capture is a checker error; executor tasks run on pool threads + the autodrain/`shutdown` semantics survive. | `--parallel` |

**Status checklist** (tick as phases land):

- [x] B3.0 — wire-format airlock (single-thread, parity-preserved) ✅ **landed**
- [x] B3.1 — cores out of heap (`Arc<…Core>`, single-thread, parity-preserved) ✅ **landed**
- [x] B3.2 — `Arc<Program>` + worker-VM construction (no threads) ✅ **landed**
- [x] B3.3a — `str` crosses the airlock by value (single-thread, parity-preserved) ✅ **landed**
- [x] B3.3b — G1 module-globals checker gate (mutation reachable from `spawn` = error) ✅ **landed**
- [x] B3.3c — read-only `home` snapshot: worker module-graph reconstruction (single-thread, parity-preserved) ✅ **landed**
- [x] B3.3d — method tasks (`spawn obj.m()`) dispatch in the worker via the rebuilt graph ✅ **landed**
- [x] B3.3-threads — real OS threads behind `--parallel` ✅ **landed** (`--parallel` flag + bounded pool + condvar `recv` + flush-on-join; `Shared.update` made cross-thread-atomic)
- [x] B3.4 — cancellation + cross-thread `os.exit` ✅ **landed** (per-nursery `Arc<AtomicBool>` cancel flag tripped by the first sibling fault/`os.exit`; observed at the dispatch back-edge + a `wait_timeout` re-checking `recv`; first-fault aborts running siblings; child `os.exit` propagates its code up the join to halt the parent; `recover:`/`defer` compose)
- [x] B3.5 — nursery-local deadlock detection under threads ✅ **landed** (per-nursery `DeadlockWatch` cloned into each worker; barrier-confirm detector in the blocking `recv` — a parked worker confirms its channel empty at most once per `epoch`, and `confirms == live` ⇒ fault `deadlock` with the cooperative engine's byte-identical message; `send`/`task_finished` report progress to bump `epoch`; watch and channel `q` locks never held together. Residual hangs documented: cross-nursery / `Executor`-spanning, orphaned message, G3 saturated-pool queued task)
- [x] B3.6 — `Executor`/B5 on the pool + A3b ✅ **landed** (`WireValue::Closure` crosses a submitted
  closure by value; `Vm::wire_callable` produces it at `submit`; `--parallel` `shutdown`/autodrain farm
  the queue to the bounded pool via a shared `run_workers_on_pool` extracted from `run_parallel_nursery`;
  the checker's `submit` arm pushes a `capture_floor` so a non-sendable capture is a checker error like
  `spawn`. Cooperative drain stays inline + byte-identical. **B3 epic complete.**)

> **B3.0 landed note (for the B3.1+ maintainer):** `WireValue` lives in `src/vm/wire.rs`;
> `Vm::to_wire` / `Vm::from_wire` + the rewritten `deep_clone` (the round-trip) are in
> `src/vm/mod.rs` (search the symbols). In B3.0 `to_wire` is **total / statically infallible** — every
> `Obj` variant maps to a wire arm and the by-reference set (`Str`/`Func`/`Closure`/`Module`/`Native`/
> `Channel`/`Shared`/`Executor`) crosses as `WireValue::Handle(GcRef)` (same heap). The `Result`
> return + the `.expect()` in `deep_clone` are forward-plumbing: B3.1 swaps the `Channel`/`Shared`/
> `Executor` handle arms for the `Arc<…Core>` itself, and **B3.3 must *add* the real `Err` arms**
> (for `Module` with mutable globals / `Func` / `Closure` that can't cross a thread) — they don't
> exist yet, so don't assume a defensive fault is already wired. `from_wire` builds bottom-up and
> `Heap::alloc` never collects, so it inherits `deep_clone`'s GC-safety. 3 unit tests (`wire_*`) pin
> round-trip value-equality, map hash/order preservation, and by-handle identity; all existing
> concurrency goldens + GC-stress stayed byte-identical.

> **B3.1 landed note (for the B3.2+ maintainer):** The cores live in `src/vm/core.rs`
> (`ChannelCore`/`SharedCore`/`ExecutorCore` + `ExecState`), each a `Mutex` (uncontended at B3.1; the
> `Condvar`/cancel bits are deferred to B3.3/B3.4). The heap holds `Obj::Channel/Shared/Executor(Arc<…Core>)`
> (`src/vm/heap.rs`). The airlock now serializes *at the core boundary*: `send`/`set`/`submit` =
> `to_wire`→store `WireValue`; `recv`/`get`/`update`/`shutdown` = pop/clone `WireValue`→`from_wire`
> (net == old `deep_clone`, one round-trip). `to_wire`/`from_wire` gained `Channel/Shared/Executor(Arc)`
> arms; `from_wire` allocs a **fresh** handle onto the **same** `Arc`, so a crossed core is shared, not
> copied (`wire_shares_core_across_a_fresh_handle`, `channel_core_shared_across_handles`). **The GC
> `children()` arms were REWRITTEN, not dropped** (decision E): they lock the core and yield embedded
> `Handle(GcRef)`s via `WireValue::collect_gcrefs`. `deep_clone` still exists (spawn args/captures
> route through it) and stays total. **Never hold a `MutexGuard` across `invoke_value`** (`Shared.update`,
> `Executor.shutdown` drain loop lock→read→drop-guard→call→relock). `display` can't `from_wire` (it's
> `&self`), so `display_wire(&self, &WireValue)` renders a `Shared` box's contents. `executors:
> Vec<GcRef>` (autodrain registry) is unchanged; `shut` moved into the shared core so a `from_wire`'d
> alias can't be double-drained (`executor_core_shut_is_shared_across_handles`). B3.2 next: `Arc<Program>`
> + a worker `Vm` that runs a `spawn`'d task **synchronously** in its own heap, wire-copying args in /
> result+`out` back — still no threads.

> **B3.2 landed note (for the B3.3 maintainer):** `program: Rc<Program>` → `Arc<Program>` everywhere
> (`Program` is plain owned data — `Send + Sync`, no internal `Rc`). The worker-VM machinery lives in
> `src/vm/mod.rs`: `Vm::spawn_worker(&self) -> Vm` (fresh empty heap, shares `Arc::clone(program)`,
> copies `gc_stress`; `host` left inert — **B3.3 must thread `host` through** so workers doing file/env
> I/O don't silently diverge), and `Vm::run_task_isolated(&mut self, PendingCall) -> Result<WorkerResult>`
> which (1) lowers a `Call` task to `Lowered::{Closure,Func}` = `ProtoId` + wire'd captures/args (the
> callee is **never** crossed as a parent-heap `Handle` — the proto is in the shared `Arc<Program>`),
> (2) builds the worker, `from_wire`s captures/args into its heap, rebuilds the closure/func over a
> **fresh empty `home`** module, `invoke_value`s synchronously, (3) crosses the result + `out`/`stderr`
> back as a `WorkerResult` (decision F: per-worker buffers, not interleaved). **Everything is
> `#[allow(dead_code)]`** — decision A keeps the cooperative engine the default through B3.2, so the
> helper can't be on the live path without breaking the interleave/parity goldens; B3.3's `--parallel`
> `join_nursery` is what wires it in.
>
> **Cross-heap safety is enforced, not just documented** (both review panels flagged the silent-dangling-
> handle risk): `WireValue::has_handle()` (`src/vm/wire.rs`) detects any by-reference `Handle` leaf
> (a heap-local `GcRef`), and `Vm::ensure_crossable` rejects captures/args/**and the returned result**
> with a clean fault — so a `str`/closure value crossing today is a `RuntimeError`, not a dangling
> read. `Channel/Shared/Executor` pass (they cross as a shared `Arc`, not a `GcRef`).
>
> **B3.3 owes — status (updated):**
> - (1a) **decision G1 checker gate — ✅ DONE (B3.3b).** `Checker::check_spawn_global_mutation`
>   (`src/checker/mod.rs`) errors on a module-global reassignment reachable from a `spawn` task; flow-
>   scoped, scope-aware, transitive over the free-function call graph. Direct in-`spawn:`-block writes
>   stay caught by the pre-existing `is_captured` gate.
> - (1b) **read-only `home` snapshot — ✅ DONE (B3.3c).** `Vm::build_worker_modules` + `map_global_value`
>   reconstruct the parent's initialized module graph into the worker heap (two-pass: alloc module objs,
>   then map globals). It **snapshots, never re-inits** (re-running a toplevel would duplicate side
>   effects). The map is GcRef-safe: `Func`/`Closure`/`Module`/`Native` are rebuilt explicitly over the
>   worker's home; containers recurse element-wise so a nested callable can't smuggle a parent `GcRef`
>   across; only pure data + `Channel`/`Shared`/`Executor` cores go through the wire round-trip.
> - (2) **`str` cross-by-value — ✅ DONE (B3.3a).** Owned-bytes `WireValue::Str` arm; `ensure_crossable`
>   now lets `str` (and data containing it) cross. A **`Closure` wire arm** is *not* yet added — no live
>   path needs it (the checker blocks closures from crossing as captures/args/channel-elems; the
>   `Executor.submit` closure case is A3b/B3.6), so adding it now would be untested dead code.
> - (3) **method tasks** (`spawn recv.m()`) — ✅ DONE (B3.3d).** `run_task_isolated` lowers a method task
>   to `Lowered::Method` (recv + args by wire) and dispatches via `do_method_call` against the rebuilt
>   `module_objs` (so struct-method `module_objs[def.module_idx]` resolves). A method that blocks on
>   `recv` faults cleanly (no scheduler in a sync worker — real condvar blocking is B3.3-threads).
>
> Tests: `worker_runs_in_distinct_heap`, `worker_returns_value_and_out`, `worker_shares_program_arc`,
> `worker_crosses_str_by_value`, `worker_reads_module_global`, `worker_calls_sibling_free_fn`,
> `worker_calls_imported_fn`, `worker_calls_through_global_fn_container` (GcRef-smuggle regression),
> `worker_reconstruction_survives_gc_stress`, `worker_runs_method_task`, `worker_method_on_struct`.

> **B3.3c/d landed note (for the B3.3-threads maintainer):** the two `home`-snapshot/method-task owes
> are discharged — `run_task_isolated` is now functionally complete *except for real threads*. New
> machinery in `src/vm/mod.rs`: `Lowered` carries a `home: Option<usize>` (the callee's home index in
> `module_objs`) and a `Method { recv, name, args }` variant; `build_worker_modules` (two-pass) +
> `map_global_value` reconstruct the parent's module graph in the worker heap; `home_index` /
> `worker_home` resolve the rebuilt home (falling back to `fresh_worker_home` for hand-built test
> fixtures whose home isn't a real module). **Cross-heap GcRef safety is the load-bearing property:**
> `map_global_value` rebuilds every callable/module by hand and recurses through containers, so a
> `[fn …]` / `{k: fn …}` global cannot smuggle a parent `GcRef` (regression: `worker_*_global_fn_container`).
> **Still owed at the flip:** (a) wire `run_task_isolated` onto a bounded pool under `--parallel`
> (it is dead-code/test-only today); (b) condvar `recv` — a method/fn that blocks on `recv` currently
> faults in the sync worker (guarded, not a panic); (c) the per-task full-graph reconstruction is
> correctness-first — consider caching/sharing the read-only snapshot across a nursery's workers when
> profiling the pool. The reconstruction shares the parent's `Arc<Program>` (protos), so only the heap
> graph is rebuilt.

> **B3.3-threads landed note (for the B3.4 maintainer):** the thread-flip shipped. `Vm.parallel`
> (set by `run_file_parallel` / the `chezzi run --parallel` flag; inherited by `spawn_worker`)
> selects the engine: `join_nursery` branches to `run_parallel_nursery` (`src/vm/mod.rs`), which
> `prepare_worker`s every task against the parent heap, farms `tasks[1..]` to the bounded pool
> (`src/vm/pool.rs` — one process-wide `OnceLock<Pool>` of `available_parallelism()` threads, each
> spawned with `VM_STACK_BYTES`), runs `tasks[0]` inline on the joining thread (decision B —
> parent participates, so nested `parallel:` can't explode the thread count), waits on a
> completion condvar, then flushes each worker's `out`/`stderr` in **task order** (decision F) and
> propagates the first (lowest-index) fault. `run_task_isolated` was split into `prepare_worker`
> (parent-heap half) + `ReadyWorker::run` (thread-side half, moved across the boundary — `Vm` is
> `Send`). Blocking `recv` under `--parallel` waits on `ChannelCore.cv` (`send` `notify_all`s);
> `Shared.update` now takes a per-core `update_lock` **only under `--parallel`** so concurrent RMWs
> can't lose each other (the lost-update race the first golden caught). Worker `host` now inherits the
> parent's read-only args+env (stdin stays `Empty` — a single consumable stream isn't shared).
> **Owed at B3.4/B3.5 (deliberately not done here):** no cancellation — first fault joins-then-reports
> rather than aborting siblings, and a child `os.exit` does not yet halt the process from a pool
> thread (B3.4); **no nursery-local deadlock detection under threads** — a genuinely all-blocked
> `--parallel` nursery *hangs* (B3.5), so every `--parallel` golden is deterministic-by-construction
> and cannot deadlock. `Executor` does not yet ride the pool + the A3b `submit`-capture gate is unbuilt
> (B3.6). New goldens: `examples/parallel_shared.chz` (cross-thread `Shared` count), `parallel_channel.chz`
> (cross-thread condvar `recv` + sort). Tests: `golden_parallel_{shared,channel}_chz_matches_expected`,
> `parallel_recv_blocks_until_send_wakes_it`, `parallel_nested_nursery_on_pool`,
> `parallel_pool_task_fault_propagates`, `worker_inherits_host_args_and_env`.

> **B3.4 landed note (for the B3.5 maintainer):** cancellation + cross-thread `os.exit` shipped.
> Each `--parallel` worker `Vm` carries `cancel: Option<Arc<AtomicBool>>` (cloned in by
> `run_parallel_nursery`) + a `cancelled: bool` latch. `ReadyWorker::run_outcome` classifies each
> task into a `TaskOutcome` (`Done`/`Cancelled`/`Exit{code}`/`Fault`); the first sibling to fault or
> `os.exit` calls `Vm::trip_cancel`, and the join scans outcomes in task order, flushing `Done`/`Exit`
> output and propagating the lowest-index `Exit` (→ parent `pending_exit`, hard halt) or `Fault`.
> Running siblings observe the flag at the dispatch back-edge (`run_until` loop top, beside the
> `gc_stress` check, guarded by `!self.cancelled` so a cancelled task's `defer`s still run) and
> unwind as a `"cancelled"` sentinel the join swallows.
>
> **The cancel unwind bypasses `recover:`** (a cancelled task must die, not resume) while still
> running `defer`s — on *both* paths it goes through `unwind_deferred(base_level)` and returns,
> never reaching the handler match. This is essential: the sentinel is a plain `RuntimeError`, so a
> worker-internal `recover:` would otherwise catch it and resume (then the `!self.cancelled` latch
> would disable further observation → the task runs to completion / hangs the join). Pinned by
> `parallel_recover_inside_worker_does_not_catch_cancel` and `parallel_defer_runs_on_back_edge_cancel`.
> **Precedence:** the join prefers the lowest-index `Exit` over any `Fault` (an `os.exit` is an
> unconditional hard halt, never demoted to a catchable error); lowest index wins within a kind.
>
> **Decision-C deviation — wake-on-cancel.** Decision C suggested a per-nursery cancel *condvar* that
> `notify_all`s blocked `recv`s. Shipped instead: the blocking `recv` waits on the channel `cv` with
> a **50ms `wait_timeout` re-checking loop**. Rationale — the faulting worker cannot know which
> channel cores its siblings park on, so a pure-notify scheme needs a per-nursery registry of blocked
> cores *and still* carries the lost-wakeup hazard (risk #2). The bounded re-check **eliminates**
> that hazard by construction; the cost is a ≤50ms abort latency on a recv-blocked sibling (invisible
> to tests/users). If zero-latency cancel ever matters, revisit with a blocked-core registry.
>
> **Known limitation (deferred):** cancellation is **single-level**. A nested `parallel:` inside a
> worker creates its own independent cancel token; an *outer* nursery's cancel does not yet propagate
> into a worker that is itself blocked in its child nursery's join wait. No test depends on nested
> cancel propagation; left for a later phase if needed.
>
> New: `examples/parallel_cancel.chz` (cross-thread `os.exit` aborts a CPU sibling, exit code 7).
> Tests: `parallel_recv_blocked_sibling_aborts_on_sibling_fault`, `parallel_cpu_sibling_aborts_on_sibling_fault`,
> `parallel_defer_runs_on_cancelled_sibling`, `parallel_defer_runs_on_back_edge_cancel`,
> `parallel_recover_inside_worker_does_not_catch_cancel` (in `mod tests`);
> `parallel_child_os_exit_halts_with_code`, `parallel_os_exit_aborts_recv_blocked_sibling` (in
> `parity_tests`, via `run_file_parallel`). The partial-work tests use a `Channel` handshake so the
> victim provably starts before the trigger faults (no timing flake); the trigger faults/exits inline
> so cancel is tripped without depending on pool scheduling.

> **B3.6 landed note (for the Tier-D maintainer) — B3 epic complete.** `Executor` rides the pool +
> the A3b submit-capture gate shipped. **Checker (A3b):** the `Ty::Executor` `submit` arm in
> `infer_method` (`src/checker/mod.rs`) pushes a `capture_floor` at the current scope depth around the
> argument check, so the pre-existing `infer_ident` read gate flags a non-sendable captured binding —
> a submitted closure's captures are now gated exactly like a `spawn:` block's (`infer_closure` opens
> its scope *at* the floor, so the closure's own params/locals stay task-local). Tests:
> `submit_{non_sendable_capture,captured_closure}_rejected`, `submit_captured_{channel,int}_ok`.
>
> **VM — `WireValue::Closure` (the load-bearing arm B3.3 deferred).** A submitted closure crosses **by
> value**: `WireValue::Closure { proto: ProtoId, captured: Vec<(Box<str>, WireValue)>, home:
> Option<usize> }` (`src/vm/wire.rs`) — `proto` lives in the shared `Arc<Program>`, captures wire
> recursively, `home` is a `module_objs` *index* (via `home_index`), so the arm carries **no**
> heap-local `GcRef`. `Vm::wire_callable` (`src/vm/mod.rs`) produces it for `Obj::Closure` and
> `Obj::Func` (a bare fn = empty captures), but `submit` calls it **only under `--parallel`**; on the
> **cooperative default engine `submit` crosses the closure by handle** (`to_wire` → `Handle`, the
> pre-B3.6 behavior) so its same-heap drain shares captures **by reference** — a mutation between
> `submit` and drain stays observable, matching the interp oracle. **(Review C-01: an *unconditional*
> `wire_callable` snapshotted mutable collection captures by value and broke `VM == interp` for the
> sequential subset — decision A. Fixed by the engine gate; pinned by
> `executor_cooperative_submit_shares_captures_by_reference`.)** The generic `to_wire` still crosses
> other closures as a by-reference `Handle`, so the `Shared`-holds-a-closure path and every existing
> golden stay byte-identical. `from_wire` rebuilds the closure over the worker's reconstructed home
> (`worker_home`); `collect_core_gcrefs` (`src/vm/core.rs`), `has_handle`, and `display_wire` gained
> matching `Closure` arms (queued captures stay GC-rooted via the executor handle's `children()` under
> `gc_stress`).
>
> **VM — drain on the pool.** Under `--parallel`, `executor_method("shutdown")` marks `shut`, drains
> the whole queue under the core lock (guard dropped before any invoke — never hold the core lock across
> a task), then farms the tasks to the bounded pool via **`run_workers_on_pool`** — the farm/inline-
> task[0]/join/flush-in-order/`Exit`-over-`Fault` core **extracted verbatim** from `run_parallel_nursery`
> (a pure refactor: the nursery path is byte-identical, it just now delegates). Each executor task gets a
> fresh per-drain `cancel` flag (first fault aborts siblings, matching the cooperative inline `r?`) but
> **no `DeadlockWatch`** — an `Executor`-spanning deadlock is an accepted hang (decision D). The
> program-exit autodrain reaches this automatically (`drain_live_executors` calls `shutdown`). The
> **cooperative default engine keeps the inline FIFO drain, byte-identical** (decision A oracle). New:
> `examples/executor_pool.chz` (`submit` ×3 → pool drain → sort; same output on both engines). Tests:
> `golden_executor_pool_chz_matches_expected`, `executor_submitted_closure_captures_by_value`.
>
> **Known load-sensitive test:** `parallel_defer_runs_on_cancelled_sibling` (a B3.4 cancel test) is a
> low-rate timing flake **only under heavy full-suite parallel load** (shared process-wide pool
> saturation delays the trigger fault); it passes in isolation and on clean HEAD with the same
> variance — not a B3.6 regression. Re-run if it trips.

---

## 5. Risk register (carry across sessions)

1. **Module globals across threads** (decision G1) — ✅ **resolved: Option A** (globals read-only after
   init under `--parallel`; cross-task mutation via `Shared[T]`, mirroring the `Ref`-non-sendable rule).
   The **checker gate is implemented (B3.3b)** and the **read-only `home` snapshot is implemented
   (B3.3c — `Vm::build_worker_modules`)**. Known gate gaps (documented in the walker comment, land with
   the flip): a module-global *closure* used as a `spawn` target (`g := fn():…; spawn g()`), and
   method-mediated call chains — both indirect-dispatch the static call graph can't follow. (The
   *runtime* reconstruction now handles a module-global closure/dispatch-table correctly; the gap is
   purely the *checker's* G1 reachability analysis through those indirect targets.)
2. **Condvar-blocked-recv cancellation** (G2) — lost wakeups; the dual-wait (channel cv + nursery
   cancel cv) must be a re-checking loop.
3. **Pool starvation** (G3) — parent-participates + documented rule for v1.
4. **Output contract** (G4) — settled (decision F) but pervasive; thread it through B3.2/B3.3.
5. **Arc-cycle leaks** (G5) — documented limitation, no fix.
6. **Deadlock counter races** (G6) — test the false-positive case explicitly.

---

## 6. Critical files (by area)

- `src/vm/mod.rs` — `deep_clone` (→ wire), `join_nursery`/`run_scheduler`/`pick_runnable`/`run_child`,
  the `recv` suspend path + `channel_method`/`shared_method`/`executor_method`, root collection
  (`collect`/GC roots), `pending_exit`/`out`/`stderr`/`native_reentry` fields, the dispatch loop
  check sites.
- `src/vm/heap.rs` — `Obj` enum + `children()` (the Channel/Shared/Executor arms `:273–275`),
  alloc/mark/sweep.
- `src/vm/value.rs` — `Value` / `GcRef(pub u32)` (`:13`) — the source of `!Send` and the basis for the
  `WireValue` mirror.
- `src/native/os.rs` — `request_exit` (`:42`) / cross-thread exit-code propagation.
- `src/main.rs` — `--parallel` flag plumbing, mirroring `--interp` (`:184`); engine selection.
- `src/checker/mod.rs` — A3b: the `Executor.submit` signature (`executor_method_sig`) + the existing
  `spawn`-capture sendability gate pattern (`capture_floors` + the `infer_ident` read gate) to mirror.

---

## 7. Out of scope for B3

B4 (real `Shared`) and B5 (real `Executor` pool) are **folded into** B3 here (B3.4–B3.6) rather than
sequenced after it, because under shared-nothing threads they *are* the same machinery. The
**alternative bet** B3 was weighed against — Tier-A-only (richer cooperative scheduler, no real
parallelism) — is **not pursued**: B1/B2 already shipped the cooperative engine, and the remaining
demand is genuine multicore. Items deliberately *not* in B3–B5 live in the
[`concurrency.md` "Deferred / backlog"](concurrency.md#11-deferred--backlog-not-b3b5) section.

**I/O-bound concurrency is explicitly NOT a B3 goal.** B3 delivers CPU-bound multicore; a blocking-I/O
task pins its pool thread (risk G3). Smart I/O handling — an elastic blocking pool (cheap) or a full
**M:N scheduler + async-I/O pollset** (for massive-connection scale) — is the **Tier-D post-B3
frontier**, designed in [`concurrency.md` §10 "Tier-D"](concurrency.md#10-future-evolution) (why
Chezzi's bytecode-VM + share-nothing-GC architecture is unusually well-positioned for it, what is
genuinely hard, and the checklist for making `--parallel` the default). Not scheduled.
