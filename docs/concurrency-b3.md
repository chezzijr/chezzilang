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
   - The heap holds only a **handle** (`Obj::Channel(Arc<ChannelCore>)`), and the `children()` arm for
     `Channel` is **dropped** (nothing in *this* heap to trace — the core holds `WireValue`, no
     `GcRef`).
   - `send` = lock + `push_back(to_wire(v))` + `notify_one`; `recv` = lock + `pop_front`-or-
     **`cv.wait` (real OS-thread blocking)**; `try_recv` = lock + pop-or-`None`; `len` = lock + len.

4. **`Shared` core moves OUT of the heap** (this **subsumes B4**). `Arc<SharedCore { Mutex<WireValue> }>`;
   the heap holds the handle, `children()` `Shared` arm dropped. `get`/`set` lock + wire-convert;
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

- **Per-thread GC never traces into a core.** After B3 the heap holds `Obj::Channel(Arc<ChannelCore>)`
  (etc.); `children()` returns nothing for it (cores hold `WireValue`, no `GcRef`). The heap object is
  still reachable via its handle (so the `Arc` isn't dropped while reachable), but tracing **stops at
  the `Arc` boundary**. The `Channel`/`Shared`/`Executor` arms in `src/vm/heap.rs:273–275` are
  removed. Clean.
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

1. **Mutable module globals across threads.** `do_spawn_block` passes `home` (the module-globals
   object) **by handle** today; under threads that `GcRef` points into the *parent* heap and cannot
   cross, and mutable globals can't be trivially wire-copied (mutations wouldn't propagate back).
   Likely forces a language decision: **module globals immutable after init** (a spec restriction) or
   per-worker snapshots (a semantic change). **Must be resolved before B3.3.** Highest chance of a spec
   change and multiple attempts.
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
| **B3.1** | **Move Channel/Shared/Executor cores out of the heap** into `Arc<…Core>` holding `WireValue`; drop the `children()` arms (`heap.rs:273–275`); the cooperative scheduler's `pick_runnable` (`mod.rs:2827`) polls `ChannelCore` length via the lock instead of `Obj::Channel(q).is_empty()`. Still single-thread, still cooperative. | Same goldens green incl. `channel_block.expected` and the all-blocked deadlock golden; GC-stress still green (parked fibers + cores survive). | **unchanged** |
| **B3.2** | **`Arc<Program>` + worker-VM construction (no threads).** `spawn` builds a fresh worker `Vm` sharing `Arc<Program>`, runs the task **synchronously** in it, wire-copies args in and result + `out` back. Resolves the worker-VM/heap-handoff plumbing in isolation. | Goldens green; a unit test proves a task runs in a distinct heap and its result/`out` come back correctly. | **unchanged** |
| **B3.3** | **Real OS threads behind `--parallel`.** Bounded pool (decision B), condvar `recv` (decision C blocking), buffer-flush-on-join (decision F). Cooperative engine stays the **default**. **Resolve decision G(1) — module globals — first.** | NEW `--parallel`-only goldens that are deterministic-by-construction (collect→drain→sort→print) + order-insensitive (set-of-lines) assertions; every existing golden stays on the default engine and stays green. | **`--parallel` new** |
| **B3.4** | **Cancellation + cross-thread `os.exit`.** Per-nursery `cancel` flag, condvar wake-on-cancel, exit-code propagation up the join (decision C). | First-fault-aborts-running-siblings; a child `os.exit` halts the process with the right code; `recover:`/`defer` still compose. | `--parallel` |
| **B3.5** | **Nursery-local deadlock detection under threads** (blocked-count vs live-count, decision D). | Port the all-blocked deadlock golden to `--parallel`; a near-miss (one sibling that *does* send) must NOT false-positive. | `--parallel` |
| **B3.6** | **`Executor` / B5 on the pool + A3b submit-capture sendability gate.** Submitted tasks run on pool threads; the checker now gates `submit`'s closure captures like `spawn` does. | `submit` of a non-sendable capture is a checker error; executor tasks run on pool threads + the autodrain/`shutdown` semantics survive. | `--parallel` |

**Status checklist** (tick as phases land):

- [x] B3.0 — wire-format airlock (single-thread, parity-preserved) ✅ **landed**
- [ ] B3.1 — cores out of heap (`Arc<…Core>`) ← **next session starts here**
- [ ] B3.2 — `Arc<Program>` + worker-VM construction (no threads)
- [ ] B3.3 — real OS threads behind `--parallel`
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

---

## 5. Risk register (carry across sessions)

1. **Module globals across threads** (decision G1) — may need a spec change; gate B3.3 on it.
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
