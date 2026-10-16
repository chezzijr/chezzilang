# Gaps archive: serial-engine removal session log (moved verbatim from `docs/history/bug-hunts/gaps-archive.md` lines 1068-1252)

## Session log — 2026-08-16 (serial-engine removal: 2 rows closed, 1 dissolved, 6 fixes found doing it)

The `--serial` cooperative engine, the `--check-parity` flag, the cooperative scheduler and every
per-engine fork were **removed** (`docs/design/roadmap.md` §2b — which shipped **pre**-freeze, reversing its own
"not before the JIT freeze" ordering). What follows is what the removal closed, and what it turned up on
the way. Every item below has a row in the OPEN ITEMS table.

**Closed BY the removal (nothing fixed — the divergent engine is gone):**
- **N10** — closed **for the cooperative fiber**. The surviving `op_wait_poll` inline-sleep arm is
  reachable on M:N and still sleeps to the deadline without re-polling siblings; see that section.
- **N6g** — dissolved. The C5 park limitation itself survives; only the divergence died.
- **W6-9r item 1** — its WON'T FIX rested on the ~60 cross-engine compare sites being scheduled for
  deletion. They were deleted. **Item 4 (the `captured()` DISPLAY boundary) stays OPEN on its own
  terms** — it is a property of the capture surface, not of the comparators.
  *(Superseded later the same day: item 4 was CLOSED 2026-08-16 — `chezzi test --show-output` writes
  `TestReport.bytes` byte-exactly. Kept as written because it records why the removal did not close
  it; the index row is the authority.)*

**Found and fixed WHILE doing it — four bugs the removal did not cause:**

### An `Executor` join waited for the joiner's own job — a healthy run reported a deadlock (`3f293c60`)

```chezzi
import std.concurrency
fn stop(e: Executor):
    e.shutdown_now()
fn main():
    ex := Executor()
    ex.submit(fn(): print("A"))
    ex.submit(fn(): stop(ex))
    ex.submit(fn(): print("C"))
    ex.shutdown()
    print("end")
main()
```

| binary | result |
|---|---|
| `target/debug/chezzi`, 60 runs, PRE-fix | **51× `A C end` rc=0, 9× (15%) `A C` then `runtime error: waiting for this Executor's jobs: deadlock` rc=1** |
| `target/release/chezzi`, 40 runs, PRE-fix | 40× `A C end` rc=0 — timing masks it |
| **CPython 3.14.6 `ThreadPoolExecutor` equivalent** | `A C end` rc=0, **every run** |
| post-fix, debug and release | **240/240 each, zero faults** |

Both `A` and `C` print first, so every job ran. **Root cause:** `join_eager_jobs` waited for
`outstanding()` to reach 0 **even when the waiting thread was itself one of the counted jobs**, so its
`PartyWait::Join` was never satisfiable and `quiesced_only_joins` declared a completed run dead. The
graceful `shutdown()` variant faulted **8/8** — the 15% figure above only looks like a race because the
`shutdown_now` cancel rung gives most runs an escape the graceful path does not have. **Fix:** discount
the joiner's own slot (`slack`) in the wait loop and in `Join` satisfiability — a self-join takes no
slots. The bounded `KNOWN_FLAKE` retry in `tests/executor_reentrant_shutdown.rs` was deleted with the
bug, since a test that retries past a real fault is papering over it.

**This is a fourth instance of the [[parked-is-not-stuck]] family** (with `W7-12`, `W7-56`, `W7-58`): a
runtime verdict that declares user code broken must be built from what is **impossible**, not from what
looks idle. Here the predicate could not have been satisfied by any schedule, because the party it was
waiting on was the party doing the waiting.

### A self-shut `Executor`'s slots were never reduced — buffered output lost, and a failing `assert` returned `Ok(())` (`3fb223ee`)

`drain_live_executors` read `shut` as "already handled", so an executor shut down from inside one of its
own jobs never had its outcome slots reduced. Two observable halves, **both red pre-fix, 150/150 after**:

- buffered job output **lost 10/10**;
- **a sibling job's failing `assert` made the run return `Ok(())`** — a silent wrong result on exactly
  the path a test runner reads to decide pass/fail. This is the worse half by a distance.

Fixed with `ExecutorCore::unreduced`, carrying an acknowledged `ponytail:` comment naming its ceiling (a
narrow window between setting `shut` and marking `unreduced`). Review verified `unreduced` is set and
cleared coherently and cannot double-reduce. A follow-up pass (`a084f3ca`) narrowed the bail-clear to
genuine self-joins, watchdogged the new test (it needs ≥2 pool threads, and hung >120 s under
`taskset -c 0` without one), and moved `executor_results_are_not_retained` to its own process after
root-causing its flake to `vm::pool` sharing rather than to the cap.

### The `EagerState` doc claimed an ordering guarantee that never existed — **the doc was the defect** (`3fb223ee`)

Filed as a runtime ordering bug **twice**, and both times the direction was wrong. `EagerState` was
documented as giving submission-ordered job output. Under `chezzi run` it never did and never could:
`HostConfig::stream` is written at exactly **one** site (`src/main.rs`, the `run` subcommand) and sends
every `print` to the real fd **at print time**, engine-wide. The submission-ordered per-slot flush
governs only the **buffered sink** that the in-process test helpers use.

| measured, with REAL work in each job | result |
|---|---|
| Chezzi nursery, `chezzi run` | printed **reversed 100/100** |
| CPython `ThreadPoolExecutor` | held submission order **0/30** |

So Chezzi already matched its owning ancestor and only the documentation disagreed. Corrected in
`src/vm/core.rs`, `docs/reference/concurrency.md` and `docs/reference/stdlib.md`.

**Why it was mis-diagnosed twice, which is the transferable part:** the control program used *trivial*
job bodies, where submission order survives by accident. **Before filing an ordering bug, run the
ancestor with realistic work** — a short task hides the very nondeterminism you are testing for.

### `List[Any]` widened an int to a float, at five positions (`ee13cf6f` → `5883dc98` → `da3a87fa`)

The tracked wave-4 gap (`Session log — 2026-07-23`), fixed. `xs: List[Any] = [1, 3.0]` stored `1.0` for
the int; CPython keeps `[1, 3.0]`. The type-blind `literal_numeric_mix` peephole widened an untyped int
const whenever a float const sibling existed, regardless of the checker-resolved element type — a
check-clean silent wrong value, invisible to any engine comparison because both engines had it.

Two things worth carrying forward:

1. **Grepping the SHAPE found a FIFTH position that enumeration had missed** — a struct-constructor
   argument. The four were listed from memory; the list was wrong. Same meta-finding this file already
   records under "a fix applied to SOME arms of an N-way set".
2. **The `if`/`match` scalar sibling was deliberately LEFT ALONE, and pushing back on the instruction to
   change it was correct.** Branches must unify, so the widen there is what *produces* the float that is
   later assigned into the `Any` slot; suppressing it would turn accepted programs into `branches have
   incompatible types: int and float`. **Rust rejects `if c {1} else {2.0}` outright**, so branch
   unification is ancestor-faithful — it is a different rule, not the same defect. (`Map[K, Any]` is
   invariant and rejects every mixed literal, so it has no site of the shape.)

The final shape rides **one** seam: every `expected_hint` set site feeds a single sink (`infer_kind`'s
`ExprKind::List` arm), so the checker records a per-literal verdict into a new `ListWidenTable` that the
type-blind backend consumes. `chezzi test` 552 → 565.

### …and that side table's key aliased two AST nodes — instance #4 of the span-keyed class (`da3a87fa`)

Adversarial review of `5883dc98` was pointed at the side table's injectivity **on purpose**, because
this file already tracks the hazard. It was real. `ListWidenTable`'s `carrier_key` aliased the
**synthesized variadic pack** onto a **piped list literal** — `desugar/mod.rs` gives the pack the CALL's
span, and a pipe gives that call the LHS primary's span:

```
vari([1, 3.0], 2.5, 1)          →  [[1.0, 3.0], 2.5, 1]
[1, 3.0] |> vari(2.5, 1)        →  [[1, 3.0],   2.5, 1]     # same program, different answer
```

A check-clean **silent wrong value**. Fixing it turned up a **third** shape the review had missed: two
synthesized packs in one pipe chain alias pack-to-pack
(`[1, 3.0] |> vari(2.5, 7) |> varif(1, 2.0)` → a hard `internal:` error on a valid program), which is
precisely why the fix carries the **callee's span** rather than a bool "is-a-pack" marker — two packs in
one chain are two coordinates, not one flag.

**Fixed by making the coordinate REAL, never by re-anchoring a span**: `ExprKind::List` gained an
`Option<Span>` origin (`None` for a user-written list, the callee's `witness_key_span` for the one
synthesized pack), so the key is `(CarrierKey, Option<Span>)`. Sweep result: exactly ONE synthesized
`List` node exists in the crate.

**The [[span-keyed-table-aliasing]] class now reads FOUR instances, not three: `M24-6`, `W7-49`,
`W7-43`, and this one.** That running count is the point of those rows — a checker→compiler side table
keyed on a `Span` aliases wherever the same AST is reachable under two identities (a spliced default, a
re-lexed fragment, a chained postfix link, and now a synthesized desugar node).

### The lib test suite was failing 8/8 on one assertion, and it was a pool, not the branch (`06ad7451`)

`src/vm/pool.rs` is **one process-wide `OnceLock` pool**. Under `RUST_TEST_THREADS=4` the
`chz_suite_passes` gate ran concurrently with ~4150 unrelated lib tests all contending for it, and one
embedded `tests/chz` case — `shutdown_now_interrupts_a_sleeping_job` — asserts a wall-clock bound
(`d < 1.0 s`, 3× headroom under the job's real 3 s deadline) on how fast a cancelled job's worker
notices the cancel. Measured: **8/8 full `cargo test --lib` runs failed this exact assertion**, `d`
landing 1.03–1.44 s — the cancel genuinely lands, just delayed by starvation. Filtering the test alone
inside the same binary already passed in 3.5 s, which is the fix made permanent: the gate moved to its
own binary `tests/chz_suite.rs`, and cargo runs test **binaries** one at a time.

Separately, `gc_tests::shared_box_survives_gc_stress` pinned an exact push order that **300 stress
iterations showed to be racy** (both orderings observed); relaxed to an order-independent GC-survival
check, which is what the test was actually for. **8/8 green after**, ~27.6–28 s per run.

**Standing rule this establishes: a test needing exclusive pool access belongs in `tests/`, not the lib
suite.** There are now four such binaries — `chz_suite.rs`, `chezzi_threads_cli.rs`,
`executor_results_not_retained.rs`, `executor_reentrant_shutdown.rs`.

**And what this was NOT.** An earlier characterisation of the same symptom blamed a *different* test
each run and read as branch flakiness (16/21 green over 21 runs). Re-run on a quiet host (load 0.94, 12
cores, no other session): **4153 passed / 0 failed / 2 ignored, 3/3, ~28 s per run**, against ~257 s per
run under contention — a **9× wall-clock difference** traced to a second Claude session working in the
same repo. Load contention and a real single-cause bug are indistinguishable from the tally alone; both
were present, and only one of them was fixable.

### What replaced the oracle, stated without overclaiming

`docs/design/roadmap.md` §2b promised four replacement oracles. **Two exist:** the CPython differential
(`src/difftest/`, pre-dating the removal and untouched by it) and a **new** two-worker-count schedule
differential (`tests/chezzi_threads_cli.rs`, task 5) that runs the whole `tests/chz` suite through the
built binary at the default worker count and again at `CHEZZI_THREADS=2`. **Two do not:** the Go
paired-programs differential and the seeded/interleaving M:N mode are **unbuilt and still planned**.
That is a real gap in race-finding coverage, not a formality, and it is why §2b stays in the file.

Task 5 also found that **`chezzi test` never read `CHEZZI_THREADS` at all** — only `cmd_run` did — so a
hand-measured "552/552 at both worker counts" had been a silent no-op proving nothing. Fixed with a
shared `apply_env_worker_count`, with a causal proof attached: on a documented "needs ≥2 pool workers"
repro, `CHEZZI_THREADS=1` genuinely times out while `=2` and the default fault fast. **A knob is not a
gate until you have shown it changes an outcome.**
