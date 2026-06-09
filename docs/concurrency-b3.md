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
| **B3.3** | **Real OS threads behind `--parallel`.** Bounded pool (decision B), condvar `recv` (decision C blocking), buffer-flush-on-join (decision F). Cooperative engine stays the **default**. **Resolve decision G(1) — module globals — first.** | NEW `--parallel`-only goldens that are deterministic-by-construction (collect→drain→sort→print) + order-insensitive (set-of-lines) assertions; every existing golden stays on the default engine and stays green. | **`--parallel` new** |
| **B3.4** | **Cancellation + cross-thread `os.exit`.** Per-nursery `cancel` flag, condvar wake-on-cancel, exit-code propagation up the join (decision C). | First-fault-aborts-running-siblings; a child `os.exit` halts the process with the right code; `recover:`/`defer` still compose. | `--parallel` |
| **B3.5** | **Nursery-local deadlock detection under threads** (blocked-count vs live-count, decision D). | Port the all-blocked deadlock golden to `--parallel`; a near-miss (one sibling that *does* send) must NOT false-positive. | `--parallel` |
| **B3.6** | **`Executor` / B5 on the pool + A3b submit-capture sendability gate.** Submitted tasks run on pool threads; the checker now gates `submit`'s closure captures like `spawn` does. | `submit` of a non-sendable capture is a checker error; executor tasks run on pool threads + the autodrain/`shutdown` semantics survive. | `--parallel` |

**Status checklist** (tick as phases land):

- [x] B3.0 — wire-format airlock (single-thread, parity-preserved) ✅ **landed**
- [x] B3.1 — cores out of heap (`Arc<…Core>`, single-thread, parity-preserved) ✅ **landed**
- [x] B3.2 — `Arc<Program>` + worker-VM construction (no threads) ✅ **landed**
- [ ] B3.3 — real OS threads behind `--parallel` ← **next session starts here (resolve G1 first)**
- [ ] B3.4 — cancellation + cross-thread `os.exit`
- [ ] B3.5 — nursery-local deadlock detection under threads
- [ ] B3.6 — `Executor`/B5 on the pool + A3b

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
> read (`worker_rejects_str_value_crossing`). `Channel/Shared/Executor` pass (they cross as a shared
> `Arc`, not a `GcRef`). **B3.3 owes**: (1) **decision G1 — now RESOLVED (Option A)**: globals are
> read-only after init under `--parallel`, cross-task mutation goes through `Shared[T]`; B3.3 adds the
> checker gate (a `SetGlobal` reachable from a `spawn` task = error) and wire-copies a **read-only**
> `home` snapshot into the worker instead of B3.2's fresh-empty placeholder; (2) **`str`/closure cross-by-value** — add the owned-
> bytes `WireValue::Str` arm + a `Closure` wire arm so `has_handle`-rejected values actually cross,
> then relax `ensure_crossable`; (3) **method tasks** (`spawn recv.m()`) are rejected outright in B3.2
> (`worker_rejects_method_task`) because a worker's `module_objs` is empty (method dispatch would index
> OOB) — wire them once module state crosses. Tests: `worker_runs_in_distinct_heap`,
> `worker_returns_value_and_out`, `worker_shares_program_arc`, plus the two rejection tests above.

---

## 5. Risk register (carry across sessions)

1. **Module globals across threads** (decision G1) — ✅ **resolved: Option A** (globals read-only after
   init under `--parallel`; cross-task mutation via `Shared[T]`, mirroring the `Ref`-non-sendable rule).
   B3.3 implements the checker gate (`SetGlobal` reachable from a `spawn` task = error) + read-only
   `home` snapshot. See decision G(1).
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
