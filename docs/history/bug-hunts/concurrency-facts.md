# Concurrency facts decided in more than one place (2026-10-22)

Root-cause pass over the concurrency model on main `eb4e66c4`, release binary. Method:
`docs/contributing/lessons.md` "How to find the root cause". A read-only pass listed each fact that
two or more sites decide. Five probe hunters (groups H to L) then wrote one program per disagreement,
ran it at `CHEZZI_THREADS` 1, 2 and 0 and seeds 1 to 4, and ran the Go or CPython twin. The main
loop re-ran every CONFIRMED repro. A duplicate counts as a bug only where a program shows the copies
answer differently. Repro programs: `/home/chezzijr/.cache/cprobe/<group>/`.

Severity: P0 silent wrong value or lost write; P1 hang or wrong behaviour; P2 wrong report text,
minor; doc = the docs disagree with the code.

## History (waves 16 to 23)

| Fact | P0 | P1 | Recurrence |
|---|---|---|---|
| Airlock: can the parent see this write? (D4) | 1 | 14 | TICKET-247 was the eleventh ticket on the fact |
| Executor: job state and run end | 2 | 8 | about 25 patches; TICKET-232 caused P0 w22 B1 |
| Owner must stop / whose fault unwinds | 0 | 6 | 188, 195, 200; E1 still open |
| Deadlock verdict | 1 | 1 | 12 false-verdict tickets, 48 fix commits |
| Channel hand-off | 1 | 2 | TICKET-185 caused w19 H2 |
| Scheduler identity, fairness, width | 1 | 3 | TICKET-208 regressed width |

## Findings

| ID | Sev | Fact | Program (short) | Chezzi | Go |
|---|---|---|---|---|---|
| L-1a | **P0** | may a closure's capture cross | `f := fn(): xs.push(2)`, `Shared([f])`, a task calls `box.get()[0]()` | no fault, parent `[1]`; the same `f` through a `Channel` faults | `[1 2]` |
| I-4b' | P1 | who ends a blocked party | main's `defer` blocks in a callback (`[1].map(rd)`), a sibling panics | hangs; `boom` is never reported | `panic: boom` |
| J-leak | P1 | does a test end clean | `chezzi test --timeout=300`: test 1 times out while its nursery has a live child | test 2 (`sleep_ms(10)`) reports TIMED-OUT (0 ms) | test isolation (`13-testing-and-entry.md:51`) |
| E1 | P1 | may a joiner run fibers | a task's `parallel:` faults while a child is parked, beside a spinner | hangs at T=1 10/10, T=2 5/10 | `panic: inner boom` |
| W23-1 | P1 | is the party judged | `s.update(fn(v: int) -> int: ch.recv())`, no sender | hangs | deadlock (declined row, `gaps.md:48`) |
| I-2c | P2 | which fault the run reports | a `defer` from a normal return blocks forever, then a sibling panics | `deadlock: every task in this parallel: ...`; `boom` lost | `panic: boom` (`05-cancel.md:239-247` agrees with Go) |
| J-2a | P2 | halt order of a blocked party | `--timeout` with a `recv` inside `[0].map(r)` | the test ends on the sibling's `boom`, not TIMED-OUT; the direct `recv` twin is TIMED-OUT | `test timed out` |
| J-2c | P2 | which halt wins | an expired `--timeout` beside a nursery deadlock | `deadlock` after 2 s | `test timed out` (`04-native-core.md:21` agrees) |
| H-1b | P2 | when a pending run halt lands | a job faults while main runs a `defer` | after the defer, main prints `after f` and `loop 0`, then halts | halts after `defer start` |
| H-4b | P2 | the deadlock report site | a deadlock inside a job's `defer` | nursery text at the `Executor()` line; no `parallel:` exists | `all goroutines are asleep` |
| L-1c | doc | sendable | `07-sendability.md:33`, `:51`: native handles cannot cross | a regex `Match` and a `Writer` cross and run | — |
| J-1d | doc | may this op block | `06-blocking.md` table: socket read in an Executor job differs | it works | — |

## Families and their single source

**Family 1: a hand-off route marks nothing (L-1a).** `crossing::marks` (`src/base/crossing.rs:219`)
owns "does this rebuild mark what it allocates". The box readout stamps the reader's heap as origin
(`src/vm/airlock_out.rs:474-478`), so `marks` sees `same_heap: true` and keeps the enclosing mark.
DEC-189 says a hand-off never marks; the `Channel` route does mark. Single source: each route passes
its own `Route` to `marks`; `same_heap` compares the storing heap with the reading heap. Owner
decision: does a closure read from `Shared` mark as the `Channel` route does (ADR D4 rule 3 says
layer C covers every crossing closure)?
**Done (TICKET-285):** a box heap records the heap that stored each closure (`Heap::closure_stamp`); `WalkSrc::origin` and `box_caller_heap_id` are deleted. Grid: `tests/chz/spec/airlock_box_closure_readout_test.chz`.

**Family 2: which fault the run reports, and who delivers it (I-2c, I-4b', H-1b, H-4b, J-2c).**
`Vm::unwind_result` (`src/vm/stmt.rs:257`) ranks only an unwind that has a cause; a `defer` drain from
a normal return has none, so the verdict latch `QuiesceState::decide` (`src/vm/quiesce.rs:704-738`)
reports its own deadlock text over a recorded child fault. A pending halt is read only at
checkpoints, and nothing reads it when `deferring` returns to 0 (`src/vm/call.rs:952`). The
deadlock text and site come from `scope_err` (`src/vm/sched_core.rs:72-76`), which falls back to
the nursery text when the fiber's park site is empty. Single source: one ranking of "which halt
wins" (recorded fault, exit, deadline, deadlock) that the latch, the defer drain end and every
report read.

**Family 3: the halt ladder of a blocked party (J-2a, J-leak).** `Vm::block_halts`
(`src/vm/blocking.rs:296`) is the only ladder with the deadline rung. Copies without it: the three
`demote.rs` loops (`:128-149`, `:218-235`, `:323-346`), the parked send re-run (`channel.rs:198`),
`fault_own_deadlock`. The test-entry reset (`src/vm/exec.rs:966`) clears the deadline and the heap
limit, but not the open nursery or owned halt that a timed-out body leaves. Single source: every
wait loop calls `block_halts`; a test entry resets the whole halt state. Fixed by TICKET-286:
`Vm::blocked_halt` (`src/vm/frames.rs`) is the one ladder; the four demote loops and the send re-run
read it; `MnSched::deadline_drain` (`src/vm/mnsched_verdict.rs`) wakes a parked channel wait at the
deadline, which also closes J-2c (Family 2); `Vm::run_invoke` (`src/vm/exec.rs`) aborts the levels a
hard halt leaves open.

**Family 4: is a blocked party judged (I-4b', W23-1).** `BlockCtx::judged` (`src/vm/block.rs:439`,
DEC-136) is `native_reentry == deferring`; a callback inside a `defer` drain or an `update` closure
is never judged, so no verdict and no recorded fault ends its wait. W23-1 is a declined row. Owner
decision: may such a party be judged when a recorded sibling fault exists?

**Family 5: a joiner runs other fibers on its own stack (E1). Fixed by TICKET-287.** A faulting or cancelled fiber at its outermost loop now parks at a fiber-owned nursery that is not done (`FaultPark`, `src/vm/frames.rs`), as `park_escaped_abort` parks a `return`; a `return` or recover-scoped `?` parks the same way and carries its body `defer` fault. An owner bypass (a child fault delivered under a `recover:` inside the body) now aborts its nurseries. `mn_worker_loop_inner` reads `SchedCore::owner_scope_done` at every fiber boundary of an inline owner, and main's eager abort never runs fibers (`JoinCover::RunnersAbort`). This closes the DEC-103 ancestor residual. The grid `owner_fault_is_reported_at_every_owner_escape_child_neighbour_and_worker_count` reports in all 1890 cells. A park raised at an offload or connect resume (a child fault wakes an owner asleep in its body) goes to the join as it is; `caught_fault_woken_at_an_offload_resume_parks_before_it_waits` covers it. Its `callback_*` cells (an owner under a native frame, `native_reentry > 0`) still wait inline, and every one of them reports.

**Docs and owner rows.** `owners.md` has no row for the checker's `sendable`
(`src/checker/op_sendable.rs:149`, 8 readers); the VM twin `WireValue::has_handle` rejects only a
module, which no program can make a value. L-1c and J-1d are stale text.
**Done (TICKET-285):** L-1c is fixed in `07-sendability.md`, and `owners.md` has the `sendable` row.

## Clean (copies that agree)

Two verdict predicates (`verdict_on`, `quiesced_core_given`); the tripped-flag veto; five copies of
`wait:` arm readiness in main, a task, a callback and an `update` closure; the yield and send-rerun
park gates; the blocking-op x context grid; exit, fault and `shutdown_now` on callback twins; the
poller cancel race (312 of 312); the end-cause order (deliberate, DEC-200); the two exit cells; every
hard-halt copy; family join loops; runner width (CPU width equals T); the airlock grid for spawn,
Executor, `Task.get`, generators and map keys.

Latent (code differs, no program reached it): the second job-fault publisher
(`src/vm/worker_vm.rs:493-501`, never first); the `try_lock` join-over copy
(`src/vm/mnsched.rs:284-298`, may relate to `gaps.md` W22-1); `CHEZZI_THREADS` env warns while
`--threads` exits 1. Perf (P2): an Executor in each of 8 tasks costs about 30% more CPU at T=1-2.

## Owner decisions (2026-10-22)

| Row | Decision | Closes |
|---|---|---|
| Task fault | Keep the Trio model: the first fault cancels the siblings, the join raises it again, a `recover:` around the `parallel:` catches it | none (confirms `01-model.md` §4) |
| Halt rank | One ranking that every site reads: 1 `os.exit`; 2 the first in time of a real fault, `--timeout` or `--max-heap`; 3 a cancelled task's cleanup fault; 4 deadlock | Family 2 (I-2c, J-2c, H-1b, H-4b) |
| Judge a wait | A party in a callback inside a `defer`, or in an `update` closure, is judged when a sibling fault is already recorded | Family 4 (I-4b', W23-1) |
| Shared mark | A closure read out of `Shared`/`RwShared` by another task marks its captures as the `Channel` route does | Family 1 (L-1a) |

## Owner decision: same-task round trip (2026-10-22)

The adversarial review of TICKET-285 found that a same-task round trip loses the write silently:
`ch.send(f); ch.recv()()` leaves `xs` at `[1]`, because `recv` rebuilds `f` with a copy of its
captures. The owner keeps D4 ("never a silent lost write") and reverses the 2026-09-23 ruling of
`07-sendability.md` rule 3: a same-task round trip faults like every other crossing. The same ticket
fixes the relay and the recv-inside-a-box lost writes, and the false fault on a closure that a box
closure builds over fresh state (TICKET-285 review charges C1 to C4).
