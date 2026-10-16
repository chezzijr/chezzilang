> Source: moved verbatim from `docs/reference/concurrency.md` §9 to §11 (implementation roadmap, future evolution, deferred backlog).

## 9. Implementation roadmap (C1–C5)

> **Historical design-log.** The `**Interp** src/interp/*` bullets below record the *original* plan,
> which targeted the since-**removed** tree-walk interpreter. That engine no longer exists, and the
> serial-VM / M:N-VM split this note used to describe is also gone — the bytecode VM on its M:N
> scheduler is now the sole engine (`--parallel` is an accepted no-op alias). Read the `src/interp/*`
> references as planning history, not current paths.

> **B3's detailed execution plan lives in [`concurrency-b3.md`](../history/designs/concurrency-b3.md)** — a phased,
> multi-session breakdown (B3.0…B3.6) with the validated shared-nothing architecture, decisions, risk
> register, and per-phase TDD focus. Items deliberately *not* in B3–B5 are in
> [§11 Deferred / backlog](#11-deferred--backlog-not-b3b5).

C1–C4 deliver a complete, shippable, deterministic concurrency feature on the sequential executor; C5
is the deferrable multicore upgrade. Each milestone is **TDD**: failing tests first (unit + corpus),
then implement **lexer → grammar/conformance → AST → parser → checker → engine → tests/examples**;
`cargo test` + `cargo test conformance` + `cargo clippy` green; update `PROGRESS.md`; commit. `chan`
internal Rust identifiers may abbreviate, but the **surface type is `Channel`**.

### C1 — surface + nursery + sequential executor (interp)
Ships join semantics with side-effecting tasks (e.g. `print`). No channels yet.
- **Lexer** `src/lexer/mod.rs`: `Token::Spawn`, `Token::Parallel`; map in `keyword()` (~L134-163).
- **Conformance** `src/conformance.rs`: `symbol()` → `"SPAWN"` / `"PARALLEL"` (exhaustive — required to compile).
- **Grammar** `docs/reference/grammar.bnf`: `<compoundStmt>` += `<parallelStmt>`; `<spawnStmt>` (both forms).
- **AST** `src/ast/mod.rs` (`StmtKind`): `Parallel { body: Block }`, `Spawn(SpawnTarget)` where
  `SpawnTarget = Call(Expr) | Block(Block)`.
- **Parser** `src/parser/mod.rs`: dispatch in `parse_stmt`; `parse_parallel` (reuse `parse_block`);
  `parse_spawn` — `spawn:` → block form (compound); `parse_spawn_call` — a call expr (a simple
  statement since TICKET-241, legal as an inline body; reject a non-call with a clear message).
- **Checker** `src/checker/mod.rs` (`check_stmt`): `Spawn` is legal anywhere (M-C) — the implicit
  function/module nursery always provides a binding target; form-1 target must be a call, and the
  sendability/airlock checks on the receiver + args still apply.
- **Interp** `src/interp/mod.rs`: `Interp.nurseries: Vec<Vec<Task>>`; `Task = Call { callee, args } |
  Block { body, scope }`. `Parallel` → push a list, run the body, pop, run tasks FIFO (reuse the
  re-entrant call path), first `!e` stops siblings + propagates. `Spawn` → eval callee+args (form 1)
  or snapshot the captured scope (form 2) **through `deep_clone`**, push the task. Add
  `deep_clone(&Value)`: scalars/str trivial; list/map/set/struct/enum recursively cloned (fresh
  `Rc<RefCell>`); `Channel`/`Shared` pass by handle; closures/native → error (not sendable).
- **Tests:** parser unit; checker (spawn-outside-parallel rejected, nested parallel ok);
  `tests/corpus/accept/parallel_basic.chz`; `examples/parallel.chz` + `.expected`.

### C2 — `Channel[T]` + sendability (interp)
Ships the canonical worker/fan-out example.
- **Checker types** `src/checker/ty.rs`: `Ty::Channel(Box<Ty>)` + helper + `compatible()`; map
  `Type::Generic("Channel", [T])` → `Ty::Channel`.
- **Checker methods** `src/checker/expr.rs` (`infer_method_call`): `Ty::Channel` arm resolving via
  `native_handle_method("Channel", …)` against the file-backed `native struct Channel[T]` method table in
  `std/prelude.chz` (`send(T)->None`, `recv()->T`, `len()->int`, … — the retired bespoke
  `channel_method_sig`'s replacement); `Channel()` constructor (builtin free fn, mirror `Set()`).
- **Sendability:** a `sendable(&Ty)` predicate gating `spawn` captures + `Channel.send`; read-only
  captured bindings (reassign of a captured name inside a task = error).
- **Interp** `src/interp/value.rs`: `Value::Channel(Rc<RefCell<VecDeque<Value>>>)` + `type_name` /
  `Display`. `src/interp/mod.rs`: `eval_channel_method` (send = push_back + `deep_clone`; recv =
  pop_front else deadlock RuntimeError; len). `Channel()` constructor in builtins.
- **Tests:** non-sendable capture rejected; reassign-captured rejected; empty-`recv` errors; worker
  example golden.

### C3 — `Shared[T]` (interp)
- **Checker** `ty.rs`: `Ty::Shared(Box<Ty>)`; methods `get()->T`, `set(T)->None`,
  `update(fn(T)->T)->None`; `Shared(v)` constructor → `Ty::Shared(typeof v)`; `Shared` is sendable.
- **Interp** `value.rs`: `Value::Shared(Rc<RefCell<Value>>)`; `eval_shared_method`; `Shared()`
  constructor; passed by handle in `deep_clone`.
- **Tests:** cross-task increment via `Shared`; an in-task mutable struct is **not** sendable while `Shared` is.

### C4 — VM parity (historical)
Port C1–C3 to the bytecode engine (`src/vm`, `src/compiler`) — at the time, checked by matching the
tree-walk interpreter's output; the interpreter has since been removed.
- **Heap** `src/vm/heap.rs`: `Obj::Channel(VecDeque<Value>)`, `Obj::Shared(Value)`; `children()` GC
  tracing.
- **Ops** `src/vm/op.rs`: `EnterNursery`, `SpawnCall(argc)`, `SpawnBlock(ProtoId)`, `JoinNursery`,
  `NewChannel`, `NewShared`.
- **Compiler** `src/compiler/mod.rs`: `Parallel` → `EnterNursery` … body … `JoinNursery`. `Spawn`
  form-1 → eval args + `SpawnCall`; form-2 → compile the block as a synthetic zero-arg proto, emit
  `SpawnBlock(proto)` (sidesteps the single-expr closure limit). `Channel()` / `Shared()` → `NewChannel`
  / `NewShared`.
- **VM** `src/vm/mod.rs`: a nursery stack `Vec<Vec<PendingCall>>`; `SpawnCall` / `SpawnBlock` register
  (deep-copy via heap clone); `JoinNursery` runs each pending task to completion using the **existing
  re-entrant call path** (the same one list HOFs `map`/`filter` use to call back into Chezzi).
  `core_method` arms for `Obj::Channel` / `Obj::Shared`; a VM `deep_clone` over heap objects.
- **Tests (historical):** every C1–C3 example ran identically under the VM (default) and the
  since-removed `--interp` flag; that differential assertion is gone with the interpreter.

### C5 — what's left, divided

C5 splits into **Group A** (small refinements that work on today's *sequential* executor — no engine
rewrite) and **Group B** (the real concurrency engine — a multi-session epic). Group A is independent
and shippable now; Group B is gated on **B1**. The surface of `spawn` / `parallel:` / `Channel` /
`Shared` / `Executor` is **unchanged** throughout.

**Group A — sequential refinements**

| # | Item | Status |
|---|------|--------|
| **A2** | `Executor` **program-exit join** — wait for any executor never explicitly `shutdown`-ed at a clean exit (creation order; `os.exit` skips it; a faulting program is not joined). Covers an executor created inside a task (W7-5b). | ✅ **done** (see [§8](#the-escape-hatch-c5-executor--a-separately-owned-work-queue)) |
| **A3a** | Reject a non-sendable **read through a nested closure** inside a `spawn:` block. | ✅ **enforced for a non-sendable local** — emergent from the persistent `capture_floors` + the `infer_ident` read gate. **Updated (B3.3 / Task 2a):** a plain **closure** read through a nested closure is now *accepted* (closures cross by value), so the pin is `read_captured_capturefree_closure_through_nested_closure_in_spawn_block_ok`. |
| **A1** | `Channel.try_recv() -> T?` — a **non-blocking poll** (`?v`/`None`, never blocks/faults/suspends). Originally deferred (its motivating mid-flight-producer scenario needed the engine), un-deferred once B1/B2 landed. | ✅ **done** (it never suspends, so it is schedule-independent — see [§5](#5-channelt--a-mailbox-outside-every-heap)). |

> *Dropped from Group A, shipped in B3.6:* **A3b** (`Executor.submit` capture sendability gate). The
> submitted closure now crosses **by value** (`wire_callable` → `to_wire`), so a
> non-sendable capture (a live generator, a native handle) faults at submit, the same way regardless
> of worker count.

**Group B — the real engine (deferred epic)**

| # | Item | Status |
|---|------|--------|
| **B1** | **Suspendable execution** — make the engine loop resumable. The fiber core; **everything else in B gates on it**. | ✅ **VM done** · 🚫 interp (non-goal) |
| **B2** | **Cooperative scheduler** — task interleaving; real **blocking `recv`** (replaces the deadlock-detect fault); mid-flight producer↔consumer. | ✅ **VM done** · 🚫 interp (non-goal) |
| **B3** | **Tier-C OS-thread multicore** — per-thread heap + GC; true parallelism. An *alternative bet* to B1/B2 (the one taken). | ✅ **VM done** (B3.0–B3.6; superseded by Tier-D D1/D2's M:N fibers) |
| **B4** | **Real `Shared[T]`** — owner-task + channel (today single-thread-serialised, already correct). | ✅ **VM done** (folded into B3.1/B3.4) |
| **B5** | **Real `Executor` background pool** — actually-backgrounded tasks + graceful exit drain under real concurrency; plus A3b (submit-capture gating). | ✅ **VM done** (B3.6 + A3b) |

**Dependency:** Group A is independent and shippable; Group B is gated on **B1**. A2 is unchanged
after B lands; A3a becomes load-bearing (not merely emergent) once captures truly cross threads.

#### B1 + B2 as shipped on the VM (cooperative fibers + blocking `recv`)

Rather than the full recursive-`eval` rewrite, the **bytecode VM** got suspendable execution cheaply
because `run_until(base_level)` is **frame-count driven** (`while frames.len() > base_level`), not
host-recursion driven: a fiber's saved frame stack *replays* on resume via ordinary `Return` opcodes —
no host call stack to rebuild. The design (all in `src/vm/mod.rs`):

- **Suspend = rewind-and-retry at an instruction boundary.** A `recv` on an empty channel, under an
  active scheduler and outside any native callback, re-pushes the receiver, does `ip -= 1` so the
  `CallMethod(recv)` re-executes on resume, and sets `self.suspend`. `run_until` and every re-entrant
  call site (`run_proto`, `do_call`, the struct-method / function-field / channel dispatch) break out
  **without** running defers, returning control to the scheduler. No mid-instruction state is saved.
- **Nursery-local cooperative scheduler.** `JoinNursery` parks the joining (parent) fiber's context
  and runs the spawned tasks as child `Fiber`s, each owning a full `FiberCtx`
  (`frames`/`stack`/`call_depth`/`cur_base`/`handlers`/`nurseries`/`fault_trace`) swapped in/out around
  scheduling. A child that never blocks runs to completion FIFO (so non-blocking programs are
  byte-identical to the old sequential drain); a blocked child parks and a runnable sibling runs; a
  sibling's `send` makes it runnable again. All-blocked-none-runnable ⇒ deadlock fault. Nested
  `parallel:` recurses into a fresh scheduler level. Parked fibers are GC roots.
- **Native-reentry guard.** A `recv` reached inside a native callback (list HOFs, `sort`,
  `compare`/`hash`/`str` hooks, `Shared.update`, the executor drain, a `defer`red call) cannot park —
  that loop/recursion state lives on the host stack — so it faults `deadlock` instead (v1 limitation).
- **`std.os.exit` in a child** aborts its siblings and the program (rides the existing `pending_exit`
  hard-halt path, which stays VM-global, not per-fiber).

**Decision (historical) — interp B1/B2 was a deliberate NON-GOAL.** The tree-walking interpreter (since
removed) was kept frozen at the **sequential concurrency subset** as the differential-testing parity
oracle for the non-blocking language surface — its value was catching VM / GC / compiler bugs, not
running concurrent workloads. Suspendable execution would have required stackful coroutines or a full
CPS rewrite of `eval`, a cost the oracle did not need. **The VM is the sole engine**, and since
2026-08-16 it has a single scheduler (M:N) — the cooperative `--serial` one was removed too
(`docs/design/roadmap.md` §2b), so there is no cross-engine parity contract left at all. (Historically, while
the interp existed, a **blocking `recv` was VM-only** — under the old `--interp` it faulted
`deadlock`.)

**Landed (VM):** **B3** OS-thread multicore (the alternative bet, taken — B3.0–B3.6), **B4** real
`Shared`, **B5** real `Executor` pool (+ A3b) — then **Tier-D** rebuilt `--parallel` as an M:N
work-stealing scheduler, **complete through D6** (D0–D6 + owes #1/#2/#3; epoll/`std.net` netpoller
landed). Blocking `recv` inside a native callback (**D5 owe #3**) is **resolved** — see below.

**Cross-nursery wakeups — M:N RESOLVED.** A fiber in an outer nursery being woken
(and *run*) by an inner one (the circular outer-sibling case — `examples/parallel_cross_nursery_circular.chz`)
is **fixed under `--parallel`** (the M:N engine): one VM-global `MnSched` with a `Vec<JoinScope>` flat
scheduler (each nested nursery is a scope enlisted into the same global run queue, with a scope-scoped
owner stop), plus early-enlisting an outer nursery's siblings so a nested owner — draining the GLOBAL
queue — runs them. The fix also routes the inline outer-body's own `send`/`close` through the held sched
(so they wake an enlisted, parked sibling), runs a `spawn:` issued *after* the enlist, and makes the
enlist atomic — see §11 below and [`docs/history/designs/cross-nursery-flat-scheduler.md`](../history/designs/cross-nursery-flat-scheduler.md).
(The cooperative engine serialized nested nursery levels and faulted `deadlock` on the same program; the
"cooperative flatten" that would have fixed it was never built and is now moot — that engine was removed
2026-08-16.)

---

## 10. Future evolution

- **M-C — implicit nurseries (shipped).** The original model (**M-A**) required every `spawn` to sit
  inside an explicit `parallel:`, *including* top level. **M-C** makes **every function body (and the
  module top level) an implicit nursery** that joins at its `return`/end (the module top level joins
  at program exit), demoting `parallel:` to an explicit *inner* sub-nursery for earlier joins. A bare
  `spawn` is now legal anywhere in a function; it is ergonomic ("spawn anywhere"), uniform (no
  top-level/function asymmetry), and still safe via the function-boundary rule (a task can't outlive
  the function that spawned it).
  - **Join semantics.** `return <value>`, fall-through end, and a `?` early-return are all **join
    points**: the function's spawned tasks run to completion FIFO, *then* control leaves. An explicit
    inner `parallel:` joins earlier at its dedent; a `return`/`?` that *escapes* an inner `parallel:`
    still cancels that inner nursery (silently — §2c1) while joining the function's implicit one.
    An uncaught **fault** propagating out of a body cancels the implicit nursery's tasks (abnormal
    exit, not a join). `defer`s run *after* the implicit join (tasks complete, then cleanup) — and
    identically for an **explicit** `parallel:` block: a `defer` directly inside the block flushes
    *after* the block's dedent join (its spawned children run to completion first, then the block's
    deferred cleanup), same order as the implicit body nursery. Each nursery on the unwind path is
    reclaimed separately, innermost-first.
    [**§2c1, 2026-08-14:** the cancel is now SILENT. This used to write one
    `"{n} pending task(s) cancelled on early exit from parallel:"` line per nursery; with eager start
    there are no unstarted tasks to count and any residual number would be racy, so the report is
    deleted — `trio` and `asyncio.TaskGroup` print nothing here either. Earlier history: resolved
    2026-06-12, when the VM dropped these reports while the tree-walk interp printed them.]
  - **Zero-overhead gate.** A body gets an implicit nursery only if it lexically contains a bare
    `spawn` (a compile-time pre-scan, `compiler::block_has_bare_spawn`); bodies without one emit
    byte-identical bytecode to pre-M-C. Implemented as a single join site — the compiler emits the
    opening `Op::EnterNursery` and flags the `Proto`; the VM's `do_return` joins for `return`/`?`/end.
    Implementation: `src/{checker,compiler,vm}`; tests in `vm::tests::implicit_nursery_*` +
    `examples/implicit_nursery.chz`. The pre-scan descends into `recover:` bodies, the one
    expression form carrying a statement block (`compiler::expr_has_bare_spawn`), so a bare `spawn`
    reached through a `recover:` still gates its enclosing frame; a closure body opens its own
    implicit nursery at its own closure compile site, never the enclosing frame's (TICKET-040).
- **Real concurrency (C5):** the Tier-A cooperative scheduler and/or Tier-C OS threads — true
  multicore and mid-flight task communication, behind the unchanged surface.
- **The `Executor` escape hatch (C5):** the separately-owned work queue for tasks that must outlive
  their scope — `submit` detached work, reap with `defer ex.shutdown()` (graceful) or `shutdown_now()`
  (cancel); program exit drains. Keeps `parallel:` pure and all background work visibly owned —
  see [§8](#8-daemon--background-tasks).
### Tier-D — M:N scheduler + async I/O (shipped — D1/D2/D5; design-log below)

B3 *originally* gave **CPU-bound** multicore (`--parallel`: a bounded OS-thread pool, one worker `Vm`
per task, blocking `recv` parking the whole thread on a condvar). **Tier-D D1/D2 have since superseded
that baseline** — `--parallel` is now an M:N work-stealing scheduler of lightweight fibers (own heap,
park by `FiberCtx` snapshot, not by blocking a thread), and **D5 closed the I/O-bound gap** (blocking
natives offload to a dirty pool; `sleep_ms` rides a timer thread) so a blocking call no longer pins a
worker (the **G3** starvation, fixed). The text below records the design as it was reasoned through.

**Two orthogonal axes — don't conflate them.** *Memory model* (how tasks share data) is independent
of *scheduler* (how tasks map to cores + how blocking is handled). Chezzi already has **Erlang/BEAM's
memory model** (own heap per task, message-copy across the boundary, races unrepresentable — §2); what
it lacks is BEAM/Go's *scheduler*. Share-nothing does **not** imply "ignore I/O" — Erlang proves you can
have both. The I/O gap is engine immaturity, not a model constraint.

**You don't classify tasks as I/O vs CPU.** A real scheduler discovers it dynamically by *where a task
blocks*: a task that hits a blocking point is parked there (thread freed); a CPU task runs until it
yields / is preempted / finishes. (The one exception is opaque native calls — Erlang's *dirty
schedulers* need a `dirty_cpu`/`dirty_io` label precisely because the runtime can't see where a NIF
blocks. The Chezzi analogue is the `native_reentry` sites.)

**Two tiers of solution — pick by goal, do NOT default to full M:N:**

- **Goal A — just don't starve on a handful of blocking-I/O tasks.** *No M:N needed.* Either
  **grow-on-stall** (spawn another pool thread when all are blocked — what Go does for syscalls) or a
  **separate elastic "blocking" pool** (keep the CPU pool core-sized; route blocking I/O ops to a second
  growable pool so they never pin a CPU thread). This is Erlang's *async thread pool* / Tokio's
  `spawn_blocking`. No suspendable tasks, no pollset — just two pools + routing. **Recommended first
  step** if/when I/O matters: small, fits the current share-nothing design, removes G3.

- **Goal B — cheap *massive* I/O concurrency (10k connections) + CPU parallelism at once.** *This is
  where you genuinely need M:N:* suspendable tasks parked on a **pollset** (epoll/kqueue via `mio`/
  `polling`), multiplexed over a core-sized thread pool with per-thread run queues + work-stealing.
  Only build this if the workload actually demands that scale.

**Why Chezzi is unusually well-positioned for M:N (the two worst sub-problems are already gone):**

1. **Bytecode VM ⇒ suspend is a data snapshot, not stack magic.** A task's state is explicit data
   (`FiberCtx`: frames/stack/ip), not the C call stack — so park/resume-on-another-thread is moving
   structs, not stackful-coroutine / split-stack / asm context-switch machinery (the bulk of Go's
   runtime). The cooperative engine *already* suspends/resumes at `recv` — it is effectively an **M:1
   scheduler today**; M:N is "run several of those in parallel + work-steal + pollset."
2. **Share-nothing per-thread GC ⇒ no concurrent-GC nightmare.** Each task's heap is private and
   collected independently — no stop-the-world coordination, no cross-thread write barriers, no tri-color
   marking across threads (Go's hardest runtime engineering). The model removes it for free.

**What is genuinely still hard (the real work):**

- **Task cost** — ✅ **addressed (Tier-D D1/D2).** The old per-task full-`Vm` + heap reconstruction
  (the ~2 s in the prime demo) was replaced by lightweight fibers sharing an `Arc`'d module snapshot
  (D1) with the heap swapped into the fiber context (D2a). "The first thing to fix" — fixed.
- **Suspend at *every* yield point, not just `recv`** — including a `recv`/I/O reached **inside a native
  callback** (HOF, `sort`, `Shared.update`, executor drain), whose loop state is on the Rust stack and
  can't be snapshotted. **RESOLVED (D5 owe #3):** **Path A** moved the suspendable list HOFs into chezzi
  (`std/iter.chz` `map`/`filter`/`fold`/`reduce`, like BEAM's `Enum.map`) so a `recv` in their callback
  *parks* normally; **Path C** Go-`handoffp`-demotes the fiber to a thread for the intrinsically-native
  islands (a `recv`/`sleep_ms`/socket op / `wait` reached inside a Rust callback) instead of faulting.
  (Path B stackful was rejected.) Residual (split by TICKET-062, W10-1): for a spawned fiber a `recv`
  with no possible sender is still the *correct* deadlock, but `main` inside a native re-entry now
  blocks in place instead and *hangs* rather than faulting, since nothing can judge it.
  The same fiddly bit Go's runtime wrestles with for cgo. See [`concurrency-tier-d.md` § "D5 owe #3"].
- **Correct lock-free work-stealing + pollset wake-ups + preemption.** Preemption is the *easy* part
  here: reduction-style cooperative yield — check a "should-yield" flag at back-edges, reusing the
  existing `cancel`/`gc_stress` dispatch check sites (Erlang's model). Signal-based preemption (Go 1.14)
  is not needed. Work-stealing + pollset are medium and copyable from prior art (Tokio/Go/Rayon).

**Effort:** a multi-month subsystem, not a weekend — but *tractable*, because the foundations
(suspendable fibers, dispatch-loop check sites, private-heap GC) already exist.

**When `--parallel` can become the default** (and serial demoted to an explicit flag): gated on
(1) ✅ **B3.4 + B3.5 landed** — a deadlocked default fails loudly (cancellation + nursery-local
deadlock detection), not hangs; (2) ✅ **per-task overhead dropped** (D1/D2); (3) a still-open
determinism-contract decision — accept **task-ordered** output (decision F's
flush-on-join) as the default and demote two-engine (serial-vs-M:N) parity to a *sequential-subset* contract run under
an explicit `--serial` flag. **(Settled for the CLI, 2026-07-13 — Interactive CLI milestone:** the
per-task buffer + task-order flush is a **TEST-harness** property of the captured sink the lib helpers
run, NOT a user-facing guarantee. `chezzi run` **streams**: prints are line-atomic and cross-task order
is nondeterministic, like Python/Go/Rust.**)** The last clause of this item read *"Serial is **never
deleted** — it stays permanently as the deterministic parity oracle + reproducible-debug engine"*;
**that was overturned and the serial engine was removed 2026-08-16** (`docs/design/roadmap.md` §2b). Recorded
rather than edited away, because it is a decision this project reversed.

- **Reuse map for the implementer:** builtin method dispatch (VM `core_method` — `src/vm/mod.rs`); parameterized
  types (`Type::Generic` — `src/ast/mod.rs`; `Ty::List/Map/Set` — `src/checker/ty.rs`;
  `infer_method_call` — `src/checker/mod.rs`); the re-entrant call-into-Chezzi path (list HOFs) for
  `JoinNursery`; block parsing/scoping (`parse_block`, `exec_scoped_block`/`exec_block`, defer-scope
  markers); `Option` constructors (`some`/`none`, `alloc_enum`) for `recv`; a one-field mutable struct
  as the `Shared[T]` template.

**Go vs BEAM — the borrow decision (settled).** Chezzi *already has BEAM's memory model* (private
heap per task, message-copy across the boundary); what it lacks is the *scheduler*. Memory model and
scheduler are orthogonal axes — so take Go's scheduler *mechanics* but BEAM's *preemption + native-call
handling*, which are downstream of share-nothing and strictly simpler for a bytecode VM:

- **From Go:** the G/M/P split + per-P work-stealing run queues (`runnext` + bounded ring + global
  overflow), the `wakep`/spinning-worker wakeup with its StoreLoad barrier, and the netpoller
  (epoll/kqueue) for sockets. Scope ids on a sched are monotonic and never reissued (`ScopeTable`,
  TICKET-199): a worker holding a retired nursery's id reads an absent scope, never a newer one.
  Fairness follows Go's `inheritTime` (TICKET-199): a `runnext` pick runs on the current reduction
  slice and does not advance the worker's schedule tick, so a rendezvous pair handing each other
  through `runnext` shares one `CONTEXT_REDS` slice. When it runs out the preempted fiber goes to
  the global queue and the local ring is served, so a hand-off pair cannot starve the ring.
- **From BEAM:** **reduction-counting preemption** instead of Go's signal-based SIGURG — Go needs
  signals only because it runs native code with a *shared GC heap* (stop at any PC, find live
  pointers); a bytecode VM has a natural safepoint every dispatch and share-nothing GC, so neither
  applies. And a **dirty/blocking pool** for opaque blocking native calls (`fs`/`io`/`sleep`) instead
  of Go's syscall handoff — Go's handoff only covers syscalls the runtime itself wraps, not opaque
  user native code.
- **Rejected from Go:** SIGURG async preemption and P-handoff-as-native-call-story (both solve
  native-code + shared-GC problems Chezzi doesn't have). **Deferred from BEAM:** priority classes.

One line: *Go skeleton, BEAM brain — because Chezzi is a share-nothing bytecode VM, which is BEAM's
world.* The full per-mechanism ledger + the phased breakdown **D0…D6** (each independently TDD-able,
D0 = the O(N²) ready-queue fix from §11) live in
**[`concurrency-tier-d.md`](../history/designs/concurrency-tier-d.md)** — the companion to this section the way
[`concurrency-b3.md`](../history/designs/concurrency-b3.md) is to §9. **Status: D0–D5 + owes #1/#2 landed (the `--parallel`
engine is now a true M:N work-stealing scheduler); D6 (epoll/kqueue pollset + `std.net`) next.**

---

## 11. Deferred / backlog (not B3–B5)

Concurrency work that is real but **outside the B3–B5 multicore epic**. Recorded so it isn't lost or
reinvented; none is scheduled. (B3–B5 itself is planned in [`concurrency-b3.md`](../history/designs/concurrency-b3.md).)

- **Cross-nursery wakeups** *(RESOLVED under `--parallel` (M:N) incl. multi-level nesting + late-spawn; the cooperative-engine flatten fix is moot now that engine is removed, but a few narrow limits still open — see below)*.
  **Resolved (D0):** `wake_on_send` (`src/vm/mod.rs`) drains *every* scheduler level, so cross-level
  **wake-marking** works — a `send` in any nursery marks the blocked fiber ready wherever it parked. The
  **common case** (consumer in an *outer* nursery, producer in an *inner* one that finishes) works end to
  end: the inner nursery completes, the outer resumes, the consumer gets its value.
  **RESOLVED under `--parallel` (M:N):** the circular outer-sibling case — a fiber woken in an *outer*
  nursery being *run* by an inner one — is fixed by the flat scheduler (one VM-global `MnSched` with a
  `Vec<JoinScope>`; each nested nursery is a scope enlisted into the same global run queue; scope-scoped
  owner stop). Early-enlisting an outer nursery's siblings lets a nested owner draining the GLOBAL queue
  run them (`examples/parallel_cross_nursery_circular.chz`, `..._fanout.chz`). The full fix also routes
  the **inline outer-body's** own `send`/`close` through the held sched so they wake an enlisted, parked
  sibling (`..._inline_send.chz`, `..._inline_close.chz`), runs a `spawn:` issued *after* the enlist
  (`..._late_spawn.chz`), and makes the enlist atomic. A genuine no-sender deadlock still faults — the
  global predicate fires unless every still-incomplete scope is merely *awaiting the builder's join*
  (a live external feeder); see `MnSched::all_incomplete_awaiting_builder`. **Independent / normal
  multi-level nesting now RUNS** (the old "2+ enlisting levels" gate is gone): any depth of nested
  `parallel:` with sibling and late `spawn:`s now runs correctly — every pending outer
  nursery enlists as its own scope, and a late `spawn:` into a middle nursery runs on the held flat
  sched as a fresh trailing scope via `register_scope_seeded` (atomic register+seed under one lock,
  un-latches a stale `terminate`) so the inline owner runs it — no clobber, panic, drop, or deadlock-veto
  race. Goldens: `examples/parallel_cross_nursery_multilevel.chz`, `..._late_spawn_parked.chz`.
  **Remaining narrow limits (revisit only if they bite real programs; full brief +
  reproductions in [`docs/history/designs/cross-nursery-flat-scheduler.md`](../history/designs/cross-nursery-flat-scheduler.md)):**
  - **Contended shared channel across nested nurseries** — 2+ live receivers racing ONE channel across
    nested `parallel:` scopes is concurrent-divergent BY DESIGN: under `--parallel` delivery order is
    nondeterministic run to run, or it may deadlock-fault. It is NOT gated and NOT special-cased;
    it only must never PANIC and never HANG (completes or faults `deadlock` cleanly — see
    `parallel_cross_nursery_contended_never_panics`). The now-moot cooperative flatten (below) would
    have closed this same gap.
  - *(Historical: the cooperative `--serial` engine serialized nested nursery levels, so the same
    program faulted `deadlock` there. That engine was removed 2026-08-16, so the promised
    "cooperative flatten" is moot.)*
  - **Inline outer-body *blocking* recv (case B)** — the cross-nursery fix is **wake-side only**. The
    inline `parallel:` builder body runs with no scheduler frame, so a *blocking* `recv`/`for v in ch:`/
    `wait:` issued directly in the body (not inside a `spawn:`) still faults with a "deadlock — no
    runnable task can send" (the diagnostic points at the `spawn:` fix). Put blocking work in a `spawn:`.
  - **Eager (per-connection) nurseries** run on their OWN private `MnSched` (`activate_eager_nursery`,
    for liveness — no inline worker between Enter/Join). A cross-nursery wake in EITHER direction — OUT
    OF an eager body (child→parent) or INTO one (parent→child, or sibling-eager→sibling-eager) — is
    routed via `MnSched::wake_run_wide` (gaps.md B5, CLOSED 2026-09-09 TICKET-099 for every direction —
    goldens `parallel_cross_nursery_nested_send_to_outer_recv.chz` and
    `parallel_cross_nursery_parent_to_child_send_wakes_the_deeper_receiver`), a run-wide walk of
    `Vm::sched_registry` that replaced the old upward-only `MnSched::parent_wake` chain. Paired with a
    peer-veto deadlock predicate (`MnSched::peer_can_move`) and a cross-sched `blocked_owner_guard`
    widening so a sched blocked on a child's join does not conclude a false deadlock about itself. **As
    shipped 2026-09-09 the peer veto instead hung a genuine nested deadlock at `CHEZZI_THREADS` 2/4/8**
    (a peer whose only fiber was a join-blocked owner never satisfied its parked-victim demand); CLOSED
    2026-09-10 (TICKET-101) by splitting that demand out of the peer question alone
    (`MnSched::quiesced_core(c, require_parked)`), so a genuine nested deadlock faults again.
  - **A private NESTED eager sched's own open body (`MnSched::body_is_fiber`).** That body is run by
    a fiber of ANOTHER sched, not a dedicated drainer thread, and that fiber is already counted on its
    own sched's `running`/`runnable`/`parked_n`/`inflight`/`blocked_owners`, and every sched sits in
    `sched_registry` and `QuiesceState::eager_bodies` — so its liveness is always visible somewhere
    else. **As shipped through 2026-09-10 such a nested sched's open body vetoed every PEER's own
    genuine deadlock forever**, hanging a cousin nursery whose only child was truly stuck
    (`cousin_fed`, `nofeed_join`) and, separately, false-faulting `main` at the default worker count
    while a deep nested chain was still running (`recursive`, past the granted-`NestedDrainerSlot`
    path). CLOSED 2026-09-11 (TICKET-112): a nested sched never vetoes a peer on its own open body, it
    never faults its OWN fibers while that body is open (the body's fiber may still feed them), and
    NESTED scheds now register with the process-wide `live` count too, judged with
    `quiesced_core(c, false)` — a nested sched whose undone fibers are all parked or blocked at a
    deeper join can send nothing until another registered sched moves, and that sched counts on its
    own. **CLOSED 2026-09-16 (TICKET-125):** an outermost sched whose every counted fiber is an owner
    blocked at a nested join (`SchedCore::only_blocked_owners`) can feed nobody and has no parked
    victim of its own to demand, so it now relaxes the same way — an Executor job whose nursery's
    only undone fiber is an owner blocked at a nested join (`exec_join`, `docs/contributing/gaps.md`) completes at
    every worker count instead of hanging.

  **(Symbol note:** the old `pick_runnable` linear scan named in earlier drafts is gone — replaced by
  D0's `ready`-set.)

- **`recv` inside a native callback** *(D5 owe #3)* — **RESOLVED (both paths landed).** A blocking
  `recv`/I/O reached inside a Rust callback (list HOFs, `Shared.update`, socket ops, `sleep_ms`, a
  `wait`) once faulted `deadlock`, because that loop/recursion state lives on the host stack and can't
  be snapshotted into a fiber. (Backstory: "mooted by B3" under the B3.3 thread-per-task model, then
  **un-mooted by D2's M:N transition** — fibers park by snapshot, so a native frame breaks the chain.)
  Fixed two ways: **Path A** moved the suspendable list HOFs into chezzi (`std/iter.chz`
  `map`/`filter`/`fold`/`reduce`, like BEAM's `Enum.map`) so their callback `recv` *parks* normally
  (`d5_owe3_recv_in_iter_map_callback_parks`); **Path C** Go-`handoffp`-demotes the fiber to a thread
  for the intrinsically-native islands (`d5_owe3_path_c_*_demotes`). Path B (stackful) rejected.
  **TICKET-062 (W10-1):** the top-level `main` task inside a native re-entry now also blocks in place
  (today the `InPlace` cells of `block::mode` for `BlockCtx::OwnThread`), instead of taking the
  "fault, as before" arm main used to fall to. Residual, split by party: a **spawned fiber**'s `recv`
  with no possible sender is still the *correct* deadlock
  (`d5_owe3_path_c_recv_in_callback_no_sender_still_deadlocks`); **`main`** in that same shape now
  *hangs* instead, because it blocks without registering and the verdict declines to judge it. The lone
  surviving same-box hazard is `Shared.update`'s hold-and-wait (won't-fix by design, separate). Details
  in [`concurrency-tier-d.md` § "D5 owe #3"](../history/designs/concurrency-tier-d.md).

- **`Channel.close()` + closed-channel semantics** — **LANDED** (branch
  `feat/channel-close`). The natural complement to B1/B2's blocking `recv`: a consumer looping past the
  producer's last value used to deadlock-fault. Resolved surface (decided with the user):
  - **`for v in ch:`** — blocking iteration; drains buffered + future values, ends cleanly when
    closed-and-drained (Go's `for v := range ch`). The headline consumer form.
  - **`close()`** — idempotent, wakes every parked/demoted receiver.
  - **`send` after close → faults** `"send on a closed channel"`; **`recv` on closed-and-empty →
    faults** `"receive on a closed channel"` (drains buffered first).
  - **`try_send(v) -> bool`** — the non-blocking partner of `send` (mirrors `try_recv` vs `recv`);
    `false` = the send can't proceed: the channel is **closed** OR a **bounded** channel is **full**.
    `true` once queued. (An unbounded channel is never full, so there `false` means only closed.)
  - `try_recv` unchanged (closed reads as `None`); a `Channel[T]` is an `Iterable[T]` (TICKET-257): `for v in ch`, a
    comprehension and `List(ch)` / `Set(ch)` drain it until it is closed and empty (Rust
    `rx.iter().collect()`, Go `for v := range ch`), so `List(ch)` blocks until a sender closes it. Implementation notes in PROGRESS.md + [`concurrency-tier-d.md`](../history/designs/concurrency-tier-d.md).

- **A3b — `Executor.submit` capture sendability gate** *(✅ shipped in B3.6)*.
  `submit` ran the closure in-heap at the drain (no airlock, unlike `spawn`'s deep-clone), so a
  non-sendable capture was *benign under the cooperative engine* — gating it then would have rejected
  valid programs. It became load-bearing once captures truly cross threads, and the gate landed with
  **B3.6** (the `submit` arm pushes a `capture_floor` like `spawn`). See §9 Group B and
  [`concurrency-b3.md` §4 B3.6](../history/designs/concurrency-b3.md#4-phased-breakdown).

- **Cooperative scheduler O(N²) in the per-nursery task count** *(historical — LANDED as Tier-D D0,
  then this scheduler was itself removed with `--serial` on 2026-08-16)*. The old
  `pick_runnable` linear-scan-per-turn (lowest-index runnable, O(N²); measured 1k→1.4 ms, 10k→51 ms,
  20k→246 ms, 50k→2.34 s) was replaced by a per-nursery **`ready: BTreeSet`** of runnable child indices
  (lowest-index pop, O(log N) per turn → whole nursery O(N·log N); `src/vm/mod.rs` — `run_scheduler` +
  `Nursery.ready`). Byte-identical scheduling order to the old scan (lowest-index is the contract), so
  all goldens stayed green. (Note: this was always purely the *cooperative default* engine — the piece
  since removed; `--parallel` uses the M:N `mn_worker_loop`, which is now the only path. D0 removed the
  quadratic wall but is orthogonal to the Tier-D per-task-cost work that makes fibers green-thread-cheap.)
