# Cross-nursery wakeups — flat-scheduler fix (design + resolution)

> **Status: RESOLVED under `--parallel` (M:N).** The circular outer-sibling case (§1–§2 below) is fixed
> by the flat scheduler described in §4. The landed fix covers: the circular wakeup, the inline
> outer-body's own `send`/`close` waking an enlisted parked sibling, a `spawn:` issued *after* the
> enlist, and an atomic enlist. Goldens: `examples/parallel_cross_nursery_{circular,fanout,inline_send,
> inline_close,late_spawn}.chz`. Genuine deadlocks still fault (the deadlock predicate vetoes only while
> every still-incomplete scope is *awaiting the builder's join* — a live external feeder —
> `MnSched::all_incomplete_awaiting_builder`).
>
> **Independent / normal multi-level nesting is fully supported** (the old "2+ enlisting levels" gate is
> GONE): a `parallel:` nested inside another `parallel:` — at any depth, with sibling `spawn`s and late
> `spawn:`s into a non-outermost nursery — RUNS under `--parallel` and matches the cooperative engine.
> Every still-pending OUTER nursery early-enlists as its own scope; a late `spawn:` into a middle nursery
> runs on the held flat sched as a fresh trailing scope via `register_scope_seeded` (registers + seeds it
> atomically under one core lock — append-only slots, un-latches a stale `terminate`), so the inline owner
> runs the late task with no clobber, no panic, no drop, and no deadlock-veto race (the atomic register+seed
> closes the `runnable==0` window a SENTINEL helper could otherwise misread). Goldens:
> `examples/parallel_cross_nursery_multilevel.chz` (independent 4-level nesting) + the unit tests
> `parallel_cross_nursery_independent_3level_runs_all` / `parallel_cross_nursery_late_spawn_into_middle_runs`.
>
> **Remaining narrow limits (NOT this routing class):**
> - **Contended shared channel across nested nurseries (concurrent-divergent by design)** — 2+ live
>   receivers racing ONE channel across nested `parallel:` scopes is a racy program. Under `--parallel`
>   delivery order may diverge from the cooperative engine, or it may deadlock-fault; that is ALLOWED
>   (suspendable concurrency is VM-only / divergent by design — see `PROGRESS.md`). It is NOT gated and
>   NOT special-cased; it only must never PANIC and never HANG (it completes or faults `deadlock` cleanly,
>   guarded by `parallel_cross_nursery_contended_never_panics`). This is the same semantic gap the
>   cooperative flatten would close.
> - **Cooperative (`--serial`)** still serializes nested nursery levels → the same program still
>   faults `deadlock` there. The cooperative-engine flatten (§5 "Cooperative") is a **separate, later
>   commit**; the design below still applies. Workaround: case C (siblings in one nursery).
> - **Case B — inline outer-body *blocking* recv (§4 last paragraph):** the fix is **wake-side only**.
>   A blocking `recv`/`for v in ch:`/`wait:` issued directly in the inline `parallel:` body (not inside
>   a `spawn:`) still faults with a "deadlock — no task can ever send". Put blocking work in a `spawn:`.
> - **Eager (per-connection) nurseries** run on a private `MnSched` (`activate_eager_nursery`, for
>   liveness), so a cross-nursery wake into/out of an eager body is a separate limit.
>
> Cross-refs: [`concurrency.md §11`](concurrency.md),
> [`concurrency-tier-d.md`](concurrency-tier-d.md), `PROGRESS.md`.

## 1. The problem in one sentence

Chezzi conflates **"nursery"** with **"scheduler level"**: a nested `parallel:` (or a function that
`spawn`s — an implicit nursery) runs to completion *before* the outer scheduler regains control, so a
fiber that is woken in an **outer** nursery is marked ready but **cannot be run** until the inner
nursery joins. If completing the inner nursery *requires* that outer fiber to run, you get a circular
wait that faults `deadlock`.

## 2. Reproduction (verified outputs)

Scratch copies live at `/tmp/xnursery/` during the investigating session; re-create from here.

**A — function that `spawn`s inside → deadlock** (implicit nursery nests like an explicit `parallel:`):
```chezzi
fn inner(a: Channel[int], b: Channel[int]):
    spawn:
        a.send(1)
        y := b.recv()
        print("I got {y}")

fn main():
    a := Channel[int]()
    b := Channel[int]()
    parallel:                        # OUTER nursery
        spawn:                       # O: child of OUTER
            x := a.recv()
            b.send(x)
            print("O got {x}")
        inner(a, b)                  # inner's spawn → NESTED nursery, joins at fn return
    print("done")

main()
```
```
runtime error: deadlock: every task in this parallel: block is blocked on an empty channel
recv() and no sibling can send — the nursery cannot progress   (at inner / at main)
```

**B — function runs inline (no `spawn`) → not even concurrent** (the blocking `recv` runs on the
nursery *owner* fiber, before the join barrier, so the spawned sibling `O` has not started yet):
```chezzi
fn inner(a: Channel[int], b: Channel[int]):
    a.send(1)
    y := b.recv()                    # blocks the OWNER fiber, before the barrier
    print("I got {y}")

fn main():
    a := Channel[int]()
    b := Channel[int]()
    parallel:
        spawn:
            x := a.recv()
            b.send(x)
            print("O got {x}")
        inner(a, b)                  # inline on owner → owner blocks; O never started
    print("done")

main()
```
```
runtime error: recv on an empty channel: deadlock — nothing is queued and the sequential
executor cannot block waiting for a producer ...   (at inner / at main)
```

**C — both as sibling `spawn`s in ONE nursery → works** (the current workaround / recommended pattern):
```chezzi
fn main():
    a := Channel[int]()
    b := Channel[int]()
    parallel:                        # ONE nursery — O and I are siblings, same level
        spawn:
            x := a.recv()
            b.send(x)
            print("O got {x}")
        spawn:
            a.send(1)
            y := b.recv()
            print("I got {y}")
    print("done")

main()
```
```
O got 1
I got 1
done
```

The genuine no-sender deadlock (`examples/parallel_deadlock.chz`) must keep faulting after the fix —
it is a *correct* deadlock, not this bug.

## 3. Root cause (where in the code)

Two yield-point rules combine into the trap:

1. **Spawned children run at the join barrier, not when the owner blocks.** An inline blocking `recv`
   on the nursery owner (case B) cannot be rescued by a sibling that has not started.
2. **The scheduler runs the innermost nursery level to completion before unwinding outward** (case A).
   A nested level cannot pick an outer fiber.

Structures:

- **Cooperative engine.** `scheduler_stack: Vec<Nursery>` (`src/vm/mod.rs:503`); each `Nursery`
  (`mod.rs:731`) holds `ready: BTreeSet` + `blocked_on: HashMap<chan_ptr, Vec<child_idx>>`
  (`mod.rs:743-748`). `join_nursery` (`mod.rs:5744`) drives the **innermost** nursery's children to
  completion. `wake_on_send` (`mod.rs:6817`) — **D0's fix** — already iterates *every* level and marks
  fibers ready across levels; the residual is purely that the innermost driver won't *run* an outer one.
- **`--parallel` M:N engine.** `run_mn_nursery` (`mod.rs:5811`) + `MnSched` (`mod.rs:1041`). The run
  *queue* is already global (D4 work-stealing: `global` overflow + `try_steal`, `mod.rs:1269/1479`), but
  `MnSched.parked` (keyed by `ChannelCore` ptr) is **per-nursery**, so a `send`/`close` in another
  nursery delivers the value but does not wake across scheds (`mod.rs:5810`). Deadlock predicate
  (B3.5 / M:N): `running == 0 && runnable == 0 && parked > 0 && done < total` (`mod.rs:935`,
  `take_runnable` `mod.rs:1243`) — currently per-nursery.

## 4. Target design — nursery = join-counter, not a scheduler frame

Structured concurrency does **not** require scheduler nesting. Go, Trio, and Kotlin coroutines all run
a **flat scheduler** and layer structured concurrency on top as *join bookkeeping*. Adopt that:

1. **One flat runnable set** for the whole VM. Keep the `BTreeSet` lowest-index-first discipline so
   goldens stay byte-identical. The scheduler picks the lowest-index runnable fiber **regardless of
   which nursery owns it**.
2. **A nursery becomes a join record** `{ total, done, owner_fiber, cancel_flag }`. The owner parks
   until `done == total`. Nesting is now a *dependency*, not a scheduling boundary.
3. **Park/wake goes VM-global.** `blocked_on` (coop) / `MnSched.parked` (M:N) keyed by channel across
   all nurseries. A `send` wakes the blocked fiber wherever it lives — generalizes D0's cross-level
   *marking* to cross-level *running*.
4. **Deadlock predicate goes global.** Fault only when **no fiber anywhere** is runnable and someone is
   parked and `done < total` — so a real no-sender deadlock still faults instead of hanging.

Under this, case A runs: `I` sends a (marks `O` runnable) → parks on b; scheduler picks `O` (any
level) → recvs a, sends b → done; `I` wakes, recvs b → done; inner join satisfied; outer proceeds.

(Case B's inline-owner-blocks footgun is a *separate, smaller* question — optionally, let the owner
yield to runnable children when it blocks mid-body instead of faulting with a "deadlock — no task can
ever send". Decide whether to fix that here or leave it as "put blocking work in a `spawn`." Lower priority
than the nesting fix.)

## 5. Per-engine deltas

- **`--parallel` (do first — least code, queue already global).** Make the park set + `send_wake` /
  `close_wake` / `park_wait` routing **VM-global** instead of `MnSched`-per-nursery; keep per-nursery
  `done/total` join counters; lift the deadlock predicate to global. The D4 global run queue already
  exists, so this is mostly park-routing + predicate scope.
- **Cooperative (harder half).** Collapse `scheduler_stack: Vec<Nursery>` into a single global ready-set
  + a `Vec<JoinRecord>`. `join_nursery` stops being run-innermost-to-completion; it becomes one loop
  over the global ready-set, and `parallel:` / function-exit push + await a join record.

## 6. Invariants to preserve (test discipline)

1. **Determinism** — lowest-index-runnable-first → all goldens byte-identical.
2. **Real deadlock still faults** — keep `examples/parallel_deadlock.chz` + `d5_owe3_*_no_sender_still_deadlocks`
   green; the global predicate must fault, never hang.
3. **Structured cancellation unchanged** — first-fault cancels siblings, `defer` ordering, `?`-escape
   (B3.4). Join records carry the existing `cancel` flag.
4. **Two-engine parity** — the serial `--serial` VM and `--parallel` produce identical stdout; the serial
   VM is the reference (it has no fibers, so its sequential semantics are the parity floor).

## 7. Recommended path

1. Turn case A (and the circular shape) into a **failing-then-green** test — the north star.
2. Prototype the **flat park + wake on `--parallel` first**; prove the circular case passes and
   deadlock detection still fires; then port the model to the cooperative scheduler.
3. Milestone-sized, touches the hottest file (`src/vm/mod.rs`). Good fit for `/brainstorm` → written
   plan, or the `auto-task` workflow with the failing test as the spec.
4. On landing: update `concurrency.md §11`, `concurrency-tier-d.md`, `PROGRESS.md`, and add C to
   `examples/` as the "correct pattern" companion.

## 8. References

- D0 (cross-level wake-marking): commit `0422af6` (`wake_on_send` drains all scheduler levels).
- Per-nursery park residual: `src/vm/mod.rs:5810`.
- Doc narrative: `concurrency.md §11` "Cross-nursery wakeups".
- Genuine-deadlock example (must stay faulting): `examples/parallel_deadlock.chz`.
