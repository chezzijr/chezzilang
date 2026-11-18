# Concurrency runtime internals

How the M:N engine implements the user-facing rules in
[`docs/reference/concurrency/`](../reference/concurrency/01-model.md). Read this before you change the
scheduler, the deadlock check, the width gate or `wait:`. Each rule names its ticket, so
`git log --grep TICKET-NNN` finds the commit and test that own it.

## Runner claims

TICKET-211 (W1): one claim decides how many runners serve a scheduler, in every nesting shape
(flat, nested in a spawned task, two deep, inside an `Executor` job; outer body open or closed).
`MnSched::claim_runners` runs under the core lock wherever runnable work grows: at
`inject_or_extend` (every spawn), at `Executor.submit`, and when a body blocks. It claims runners
while fewer than `worker_count()` run and fewer fiber runners run than tasks are undone; the
body's own thread counts toward `worker_count()` but not toward fiber runners, so `--threads=1`
stays one runner. `Vm::start_runners` starts what it claimed. The claim is demand only: how many
runners run at once process-wide is the one `width::RUNNERS` budget of N, main included
(TICKET-230, see "One runner budget" below). A non-Executor runner is a RAW
`chezzi-eager-helper` thread from the `NestedDrainerSlot` thread cap — never the bounded pool,
because a body may be waiting on an `Executor` job queued behind a pool helper that runs to
global terminate (DEC-103, DEC-159). Runner threads are reused: one that finishes a nursery parks
in `src/vm/runner_cache.rs` for up to one second and serves the next nursery's runner, so 20 000
nursery rounds do not start 20 000 x `n - 1` OS threads. The inline joiner runs fibers only if
the claim has a slot left (`claim_joiner`), so the runner count stays `worker_count()`.

## The deadlock check

**One count, one check (plan 2026-11-17, Go's `checkdead`).** `quiesce::Awake` counts the tasks of
the run: `alive` (started, not ended) and `awake` among them (not parked). Both halves live in one
atomic word, so each change is one read-modify-write and every reader sees both at one instant.
The count changes only at a task's start (`Awake::task_started`, before its parent continues), park
(`Awake::asleep`, after the wait is registered, under the bucket's lock), wake (`Awake::woken`,
inside the lock hold that requeues the woken fiber) and end (`Awake::task_ended`).
`tests/owner_guards.rs` `the_awake_count_has_four_writers` fails on any other writer. The count is
its own fact: it is not a sum of a sched's `running`, `runnable` or `inflight`, so no judge walks
the scheds to decide it.

**Who is asleep.** A fiber parked on a channel, `wait:`, guard or join bucket. Main is fiber 1, so
its waits are parks like any task's. A task waiting in place is awake: a stdin read, a blocking
native, a socket, a C call, a demoted wait (an FFI callback, a map/set probe hook), as a goroutine
in a syscall keeps its M busy in Go. A timer is a task of the count while a task is parked on its
channel and it has not fired (`ChannelCore::timer_wait` / `ChannelCore::timer_fired`), Go 1.23's
rule: a timer nobody waits on (a `wait:` another arm won) keeps nothing awake, so a later deadlock
is reported at once. The wait ends at the task's resume or with its fiber.

**The order rule.** A wake adds its +1 inside the lock hold that publishes it, before the waker can
park or end, as Go's `goready` puts the woken G on a run queue before the M goes idle. A wake that
published first and counted after would let the waker's own -1 reach 0 with a live task in a queue:
a false fatal deadlock. A task that ends with wakes still to make (`drain_family` after a fault, a
timer channel's send) holds a token task across them, so its end is not the -1 that reaches 0.

**The check.** The -1 that takes `awake` to 0 with `alive > 0` (from `asleep` or `task_ended`, a
fired timer's included) calls `MnSched::on_deadlock` (`src/vm/deadlock.rs`), its one definition
(`tests/owner_guards.rs` `the_deadlock_verdict_has_one_definition`). It takes the run's
`verdict_gate`, which orders it against the `--timeout` deadline timer, a waker the count does not
hold: a deadline that passed requeues its tasks (they return the deadline at their waits) and the
check declines; once the check latches, the timer requeues nothing. It reads `all_asleep` again,
latches `RunHalt::Deadlock` (`QuiesceState::latch_deadlock`), and ends every asleep task of every
sched of the run at its wait (`SchedCore::take_asleep`, `SchedCore::end_asleep`): each adds its
listing entry at its own wait, joins included, is dropped without running a `defer`, and has the
guards it holds cut. Nothing moves meanwhile: with no task awake, no wake can happen. The root sched
ends last, so main's end lets the driver render the listing with every other task's entry in it.
`RuntimeError` carries no deadlock flag: every reader asks `run_halt() == RunHalt::Deadlock`
(`Vm::deadlocked`).

**Always fatal.** `recover:` does not catch it, and no `defer` runs after it (Go prints
`fatal error: all goroutines are asleep - deadlock!` and nothing after). A lock cycle or a re-lock
is not a fault of its own: its tasks park on the guard, and the check reports them like any other
wait. A partial deadlock (some tasks stuck while another runs) is not reported: the run hangs, as in
Go, and `chezzi test --timeout` is the backstop.

**Per test in `chezzi test`.** The count covers the run, and tests run one at a time, so a latch
during a test is that test's deadlock. Its latch id (`QuiesceState::verdict`) tells it from a later
test's. The test fails with the listing, its tasks are dropped, and a guard one of them held is
released `CUT` (`UpdateGuard::mark_cut` through `Vm::cut_held_guards` and `core::cut_all`), so a
later test that takes the box blocks at its own call, and the check lists it there. The count is back to main
alone for the next test.

The grids: `tests/awake_count_grid.rs` (each wait kind x context, live and dead, at T=1, 2, 4 and
seeds 1-3: every live cell `deadlocks=0`, every dead one `deadlocks=1`), `tests/wait_counts_grid.rs`,
`tests/deadlock_latch_race.rs` (the deadline timer against the latch, through the debug-only
`CHEZZI_TEST_DEADLOCK_LATCH_WINDOW_MS` hook) and `tests/deadlock_verdict_stress.rs`.

## Guard waits

**A fiber's guard wait parks (2026-11-08), as a goroutine's `sync.Mutex` wait does.** A contended
`set`/`update`/`write` in a fiber (the `Park` cell) retries its take a few times (a few
microseconds, as Go spins), then lists its sched with the box (`BoxLock::park_waiter`) and parks
(`ParkedEntry::Guard`). Its `WaitTicket` travels in the task's `FiberCtx` (`Vm::guard_ticket`), so a
parked writer stays counted (writer preference) and a parked reader keeps its admission; the rewound
method call re-runs and takes the box through that ticket. A release (`release_write`, the last
reader's drop, a ticket drop) wakes the box's bucket on each listed sched (`MnSched::wake_cross`; a
wake that finds the sched's core lock held hops to the timer thread). A parked guard waiter is
asleep in the run's count; an AB-BA cycle or a re-lock just parks, and the deadlock check lists it.
200 waiters at `--threads=2`:
`demotes=0` (`tests/park_scale.rs`; base 199).

A holder that the deadlock check ended never released its guard, as a deadlocked goroutine never
unlocks its `sync.Mutex`. The check cuts the guards of every task it ends (`SchedCore::end_asleep`
through `core::cut_all`; `Vm::cut_held_guards` for a task's own `held_guards`), and the release
frees the box with the `CUT` bit of its lock word set (`UpdateGuard::mark_cut`). No task takes a
`CUT` box again for the rest of the run, a waiter, a fresh acquire and a later `chezzi test` fn alike
(Go blocks every `Lock` of a `Mutex` nobody unlocks, TICKET-298). That matters only in `chezzi test`,
where the run goes on after a test's deadlock: a test fn that touches a box a deadlocked test left
cut errors at its own `update`.

The other contexts wait in place (`main`, the builder, an eager job, a fiber inside a nested native
callback). Such a waiter waits for the guard to come FREE holding no width permit, then takes the
permit, then the guard. Taking the guard first made 6 tasks x 2000 `update`s take 16-22 s at
`--threads=2`, one 5 ms timeout per handoff (TICKET-193). A fiber in a Demote context first waits in
place for up to 5 ms (`GUARD_DEMOTE_BUDGET`, `src/vm/core.rs`), then demotes its worker: demoting on
every acquire made 50 000 one-`update` fibers exhaust a 32 768-task ceiling (TICKET-016).

## A wait inside a callback

**A callback runs as a frame, so its wait parks (plan `2026-11-07-frame-reentry`).** A wait in a
hook, an operator (`a + b`, `x[k]`, `-x`, `in`, `==`, `<`), an f-string part, `print`'s `str`, a list
HOF callback (`map`, `filter`, `fold`, `take_while`, `count`, `position`, `min_by`, the
`sort_by_key` key pass), a sort comparator (`sort_by`, a struct key's `compare`, `sort()` and
`min`/`max` over structs), a `T.from_iter(it)` drain, a `Shared`/`RwShared` closure or a generator
body parks the fiber (Park), as in Go. The handler on the dispatch loop pushes the callee's frame with a `RetAction` (`CallFrame.ret`,
`src/vm/ret_action.rs`, `Copy`, 2 bytes) and returns; `do_return` runs the rest of the op through
`Vm::apply_ret`, and a frame that leaves without a return gives its state back through
`Vm::drop_ret`. A loop keeps its state in stack slots below the callback frame (a HOF
`[src, snap, acc, f, i]`, a drain `[src, list]`, `print` `[argc, i, line]`, a box `[box, f]`, a
generator its handle), so it needs no extra GC root and no unwind change. A sort is an iterative
merge (the same top-down split, walked with an explicit stack, so the comparator call order is
unchanged). A generator resume copies the body's one saved frame onto the caller's stacks and
copies it back at `yield`.

A box guard belongs to the task, not the worker: the task's `held_guards` and guard token travel in
its `FiberCtx` across a park. A task that holds a guard is preempted at the D3 safepoint like any
other (since 2026-11-08): a fiber that waits for its guard parks, as Go preempts a goroutine that
holds a `sync.Mutex`.

**What still re-enters.** A push is legal only from the dispatch loop (`flat`: every method call
op reaches it there). A hook reached from a nested container walk (`==`/`hash`/`str`/`<` inside a list, tuple, map
or set), a map/set probe, `unique`/`dedup`, set algebra, a tuple or list `sort_by_key` key,
`drain_iterable` (`T.from_iter(x)` where `x` has an `iter` hook), a `json.decode` default thunk and
an FFI callback keep `Vm::reentered`: the fiber demotes and waits in place, awake in the run's
count, so a deadlock there hangs (`gaps.md` W23-1). A `defer` drain runs on frames too (since 2026-11-07, `src/vm/defer_drain.rs`): it is a
trampoline frame that calls each record through a real call op, so a wait in a `defer` body, or in a
callback of a deferred native method (`defer xs.map(f)`), parks its task. The `--max-heap`
hard-halt drain's wait parks too (plan 2026-11-17), so its deadlock is reported. The grid is `tests/callback_park_grid.rs`: every
callback kind × `main`, a helper, top level, `spawn:` and a `test fn`, with no sender (`deadlock`,
the same text at T=1, 2, 4) and with a live sender (the golden output, no false deadlock).

## Wait implementation

**Implementation notes.** *(Done. A new `Op::WaitPoll` holds the arm operands on the
operand stack — one slot per recv-arm (the channel), TWO per send-arm (channel THEN value, walked via a
per-arm slot cursor keyed on `WaitMeta.is_send`) — polls source order, and jumps to the chosen arm's body /
`else`, handles a live `timer` arm (see below), faults send-to-closed, or parks. The M:N
park (below) files the fiber under every arm key with an `Arc<WaitPark>` token and sweeps it out of the
other buckets on resume. The park-gap re-check is **kind-aware**, classifying each arm
READY / LIVE exactly as the poll does: a recv-arm is ready with a queued value **or once closed and
drained** (D2: the poll takes it and binds `!RecvError`, built by `Vm::recv_result`, the same value a
bare `recv` returns), a send-arm with a **free slot** (`queue.len() < cap`, or unbounded) or on close.
Using the recv predicate for a full send-arm would spin requeue→re-poll→re-park. A `close()` landing in
the poll→park window is seen by the re-check (a closed arm is READY), so it is no lost wakeup (W7-2).)*

*Superseded 2026-11-04 by D2 (plan `2026-11-04-closed-channel-recv`):* before D2 the poll **skipped** a
closed+empty recv arm, which the re-check called **DEAD**; requeueing on one dead arm among live ones
spun, so a dead arm was never READY, and an all-dead re-check requeued into the all-closed fault. The W7-13r(a) rule "`closed` is not a recv arm's wake condition" and the
DEC-176 rule "judge an all-closed `wait:` as a whole" served that skip. D2 makes a closed arm a taken
arm, so `closed` is a ready condition on every wake predicate (park re-check, demote poll, eager
in-place wait), the re-run poll settles on it (no spin), and the fault is gone.

**Send-arm inside a native callback (TICKET-185).** A send-arm that cannot move its value, reached
*inside a native callback* (a `Shared.update` closure, a list-HOF), DEMOTES and blocks on its offer,
as Go blocks — like a plain full `send` there.

**Timer arm — timed-park, not inline-sleep.** A live `timer(ms)` arm is handled differently per
*waiter*. A waiter with no worker loop under it — the INLINE body of an outermost `parallel:` builder
— has no thread of its own, so it **inline-sleeps** to the soonest deadline then takes the timer arm.
**Known-limit (`docs/contributing/gaps.md` N10):** the inline-sleep fires
*before* the park, so if a **runnable sibling** could satisfy a non-timer arm, that waiter strands it
and takes the timer where a party parked on a real worker thread would take the sibling's `send`
instead (the worker-thread behavior is correct). A party with
its own worker thread must **not** inline-sleep: that would
pin the OS worker and strand a sibling `send` that lands mid-window. Instead it arms **one** background
`timer::submit_at(deadline, send_wake(true))` on the soonest timer arm's own channel (guarded by an
arm-once `ChannelCore.timer_armed` CAS so a re-park can't re-arm) and falls through to the normal
snapshot-park, so the timer is just another bucket. A waiter that OWNS ITS OS THREAD — an eager
`Executor` job, or the top-level `main` thread (with or without a native-callback frame under it) —
needs no injected wake at all: it
blocks in place with the timer as one more arm and simply **clamps** its wait to the deadline, so a
sibling value that lands first wins and the timer is taken on the re-poll otherwise
(`docs/contributing/gaps.md` **W7-14**: `timer(300)` must not beat a value that arrives at 50 ms, as
Go's `select` takes the value). The `WaitPark` claimed-CAS sweep then picks **exactly
one** of {a sibling `send`/`close` on any arm, the timer's own deadline `send_wake`} — a value arriving
before the deadline wins the wait (the value is **not** stranded), the deadline wins only if nothing else
did. The `native_reentry > 0` demote path threads the deadline into its bounded poll (channel scan first,
so a real send still beats the timer).
- *Non-blocking* (`else` present) and the *poll* step reuse the existing `try_recv` path (a timer arm's
  `try_recv` is already deadline-aware) — straightforward in the VM. **(Done.)**
- *Blocking* (no `else`) needs a **multi-channel park** — **done in both schedulers.** A fiber parks in one
  `parked[key]` bucket (`MnSched::park`/`send_wake`/`close_wake`) and is woken by a send to that key. `wait`
  needs one fiber parked on N keys, woken by the first sender, and **swept out of the other N-1 buckets** —
  otherwise a later send wakes a fiber that already moved on. **M:N implementation (landed):** a
  `WaitPark { fiber: Mutex<Option<Fiber>>, keys, claimed: AtomicBool }` held once behind an `Arc`, with a
  `ParkedEntry::Wait(token)` filed in every `parked[key]` (the bucket is now
  `HashMap<usize, Vec<ParkedEntry>>` where `ParkedEntry` is `Recv(Fiber)` or `Wait(Arc<WaitPark>)`).
  `MnSched::park_wait` does the N-key gap re-check and files all N tokens + `parked_n += 1` (ONE fiber)
  under one core-lock hold. The re-check is **kind-aware, with three outcomes per arm** (mirroring
  `op_wait_poll` exactly, or the two disagree — that was **W7-2**): **READY** (a recv arm with a queued
  value / a tripped `done_latch` / a closed channel (D2); a send arm with a free slot or a close) →
  requeue; else **LIVE**. Cancel → requeue. The re-run `WaitPoll` takes the ready arm, so a requeue
  terminates. A `close()` landing in the poll→park window wakes an empty bucket, and nothing would
  wake that key again; the re-check sees the close and requeues instead of stranding the fiber.
  (The READY / DEAD / LIVE re-check from before D2 is in the superseded note under
  **Implementation notes**.) The
  first waker (in
  `send_wake`/`close_wake`/`cancel_drain`/`flag_deadlock`) CASes `claimed`, `take()`s the fiber, and
  removes its token from every other bucket by `Arc::ptr_eq` — all under the one lock, serialized with
  `park_wait`'s gap re-check (lost-wakeup-safe). Routed via `Disp::WaitPark(Vec<(key, core)>)` captured
  while the fiber heap is live (mirrors `Disp::Park`). The single-channel `recv` park stays the **1-key
  `ParkedEntry::Recv` special case** (alloc-free, provably unchanged — regression test
  `vm_wait_single_arm_recv_park_unchanged_under_parallel`).
- *A waiter with no worker loop* (the inline outermost-`parallel:` body): poll arms once in source order; first ready wins; else if `else`,
  run it; else if any arm is timer-backed **and the waiter has no worker loop to drive a park**
  (the `InlineSleep` cell of `block::mode` for `BlockCtx::Builder { job: false }` — a party that owns
  its OS thread blocks in place with the timer clamped instead, W7-14),
  inline-sleep to the soonest deadline and take that arm; else
  fault (the existing deadlock fault; a closed recv arm is ready, D2). Deterministic → matches a worker-thread party's
  behavior **except** when a timer arm races a runnable sibling (`docs/contributing/gaps.md` N10): the inline-sleep runs before the park,
  so this no-worker-loop waiter takes the timer where a worker-thread waiter takes the sibling — a
  known limit (the worker-thread behavior is correct). Proper fix = park first, inline-sleep the timer
  only when the quiesce path would idle-deadlock.
- *`native_reentry > 0`* (inside a native callback): snapshot-park is impossible — mirror
  `demote_recv_block` with a **multi-channel demote-poll** (`demote_wait_block`: register the wait as
  one `Waiter` in `SchedCore::waiters`, poll all N queues source-order under the core lock on a bounded
  `DEMOTE_POLL_BACKOFF`). **v1 limitation (sound, lower-throughput):** there are N channel condvars and no
  single one to block on, so the demote loop polls on a backoff timer rather than waiting on a targeted
  condvar — same shape as a timer wait inside a callback. The snapshot-park (reentry == 0) is the fast
  path; the demote is only reached when a `wait` is run from inside a host-stack native callback.

## One runner budget

**The budget also preempts a CPU loop inside a native callback** (`List.map`/`sort`/`Shared.update`
closures; `docs/contributing/gaps.md` **W14-14**, TICKET-141). That fiber cannot leave its thread — the callback's
loop state is on the host stack — so the THREAD hands its runner slot to one replacement worker and
takes a slot back in FIFO order (`src/vm/width.rs`). `--threads=N` stays N
runners, like Go's `GOMAXPROCS=1`: a callback spin no longer starves a sibling, a sleeper or a
cancelling fault. It fires only when a sibling is runnable or queued, so a lone spin costs nothing.

**The budget also preempts an `Executor` job and a job's nursery (TICKET-205).** A job runs to its
end on a pool thread and cannot leave it either, so it is one more party of the same gate. Before
this fix a job's `Vm` skipped the safepoint: at `CHEZZI_THREADS=1` a job in a CPU loop starved every
other job, of its own executor or another, until the loop ended (`os.exit` from a second executor's
job landed after 23 s; Go at `GOMAXPROCS=1` exits at ~100 ms). The rules, one mechanism for every
kind of party:

- **One runner budget of N per process, main included: `width::RUNNERS` (TICKET-230).**
  `--threads=N` caps the threads that run Chezzi code at once at N process-wide, like Go's
  `GOMAXPROCS`, at every N. The gate holds `worker_count()` permits, read at every acquire, and it
  is the only width budget. Every thread that runs Chezzi code holds a permit while it runs: main
  takes one in `Vm::run` (`width::RunPermit`), and drainers, helpers, pool threads, demote
  replacements and `Executor` runners take one in `take_runnable_inner` before they pick. Before
  TICKET-230 the gate counted runners only at `--threads=1`, so each `Executor` and each sched got
  its own N: two Executors with 4 CPU jobs each used 4.01 cores at `--threads=2`, now 1.98.
  `claim_runners` stays per-sched DEMAND (how many runners a sched wants), not width. Thread counts
  have separate caps: `NestedDrainerSlot` caps drainer and helper THREADS at `worker_count().max(2)`,
  the pool keeps `worker_count()` threads plus one replacement per blocked job, and `Executor`
  runners stay outside the thread cap (a denied start would cancel the job). The gate's state is
  per OS thread, not per `Vm` or per scheduler. A party that cannot leave its thread at a slice end
  (a native callback, an `Executor` job, a nursery body on main) hands its permit over through
  `Vm::slice_end_in_place`; main does so only once it has started another runner, so a
  single-threaded program pays nothing per op. A fiber that can leave its thread yields as before.
- **A job hands its slot over only when a job waits and no pool thread is idle**
  (`pool::job_waits_for_my_slot`). K CPU jobs at `--threads=1` then run on up to K pool threads that
  share one permit: threads grow with the parties that cannot leave their thread, runners do not.
- **A gated thread picks a fiber, and draws from the seeded RNG, only while it holds the permit.**
  `take_runnable_inner` takes the permit before the pick and releases it before every wait.
  An idle unseeded worker also takes a FREE permit (`width::try_acquire`) to look for work, and
  picks whenever it holds one (TICKET-230): gating the pick on a relaxed `runnable > 0` read made
  idle workers skip work they could take (`send_one_channel` T=0 1.37 s vs base 0.76 s). Seeded
  mode keeps `holds() && runnable > 0` and takes no free permit, because `try_steal` and the
  global-first coin draw from the seeded RNG.
- **The gate grants by queue position, not by queue head (TICKET-230).** A ticket at queue position
  i is granted while `held + i < cap` (`WidthGate::acquire_slot`). A head-only grant made every
  thread wait behind a ticket reserved for a thread the OS had not run yet: `send_one_channel` T=0
  ran 29.2x base, with every gate wait taken while permits were free. At cap 1 the two rules agree.
  The free-permit path is lock-free: `held` is an atomic, and `release` locks only when a thread
  queues.
- **The waker queues the woken thread only when the budget is full.** `ChannelCore::wake_all`
  reserves a FIFO ticket for each gated in-place channel waiter (listed once per wait by
  `Vm::gated_register`), and `MnSched::notify_waiters` does the same for each gated idle worker,
  before it notifies, but only when no permit is free beyond the queued tickets (`width::full`,
  TICKET-230). Then wake order is the waker's order, not the order the OS runs the woken threads
  in; with a free permit the woken thread takes it itself. At cap 1 a waker that holds the permit
  always queues, so T=1 order is unchanged. Every runner thread is gated from birth and its spawner
  lists its slot (`spawn_runner_thread`).
- **A worker keeps its permit across fibers while no thread queues (TICKET-230).**
  `Vm::mn_worker_loop` returns its permit at a fiber boundary only when `width::handover_due` says
  so, and that needs a queued thread. Then it queues its own next turn first, while it still holds
  the permit, when a fiber is runnable or its fiber yielded (DEC-206); the `OwnTurn` guard
  withdraws a turn the loop exits without taking. A callback thread does the same in
  `Vm::slice_end_in_place`, where the acquire that follows always takes the ticket.
- **A preempted holder hands over once per quantum, not per slice (TICKET-230).** At a preemption
  slice end a holder hands over only once `width::HANDOVER_QUANTUM` (100 ms) has passed since it
  first saw a queued thread. A park, a finish, an offload or an in-place wait hands over at once.
  Seeded mode hands over at every slice end, so replay does not depend on a clock. Cost: a queued
  thread behind N CPU-bound holders can wait up to one quantum; fibers inside one sched still
  rotate every slice. Why: with more runner threads than permits, a hand-over per slice makes
  every thread run part of the time on its own core, and `schedutil` clocks those cores down. Two
  Executors at `--threads=2` used 16.9 s of CPU for 7.2 s of work; 10, 20 and 30 ms left 13-17 s,
  100 ms gave 7.2 s.
- **A preempted fiber keeps its worker when nothing else is runnable (TICKET-230).** At a slice end
  with `runnable == 0`, an unseeded worker that holds its permit and is not demoted runs the same
  fiber again on a fresh slice instead of pushing it to `global`. Go gives a P with empty queues
  its preempted goroutine back. Queueing it let an idle worker take it and run it on a CPU that was
  idle (`primes_parallel` at the default count: 1.36x wall). When another fiber is runnable the
  fiber still yields to `global`. Seeded mode keeps its replayed yield order.
- **Seeded replay is exact only where that is free (owner decision 2026-10-05).** The seed is a
  fuzzer first. A nursery join in place, a `Shared` guard wait, an `Executor` join and job start,
  and a `recv` or `wait:` inside a native callback replay at a measured rate (`docs/contributing/gaps.md`
  W15-10). A fix that makes one exact ships only if unseeded runs pay nothing for it, and never as
  a seeded-only gate, queue or park path. A direct guard hand-off (the releaser gives the guard to
  the first queued waiter) is ruled out: every contended update then costs one thread switch
  (`tests/shared_update_contention.rs` one-box source at T=1: 225 ms against 29 ms).
- **A listed idle worker never sleeps untimed**: a waker can reserve a ticket for a listed worker
  just before its sleep, and a ticket nobody takes holds a permit back. Its sleep is bounded by
  one `DEMOTE_POLL_BACKOFF` tick. A worker is listed only when fewer than two permits are free
  besides its own (`width::tight()`, TICKET-230; always true at `--threads=1`). An unlisted worker
  can receive no ticket, so it sleeps untimed on `idle_cv`.
- **A runner thread parks in `runner_cache` holding no permit and no ticket**, so gated threads are
  reused across nurseries like ungated ones were (TICKET-230).
- **Cost.** A hand-over has no CPU cost of its own, but a serialized `--threads=1` run of two CPU
  jobs costs more wall time than one sequential run when the kernel's `schedutil` governor ramps
  each thread separately: 0.90 s serial, 1.52 s with hand-overs, 0.91 s pinned to one core with
  `taskset`. A longer hand-over quantum (64 slices) measured worse (2.24 s); there is none.
  A T=1 channel hand-off costs more too: `rendezvous_pingpong` measured 1.10x pinned and 1.22x not
  pinned, accepted by the owner on 2026-10-03 (`docs/contributing/benchmarks.md` §TICKET-205).

Grid: `tests/runner_handover_grid.rs` (spinner kind x victim kind x action x worker count, 672
cells; 36 were red before the fix, all job-versus-job at one worker).
A gated thread releases its slot around every wait in place (demotes, guard waits, inline nursery
joins and aborts, inline sleeps, `Executor.shutdown()`, blocking natives). Since 2026-11-08 an
`Executor.shutdown()` in a fiber PARKS for the join (item 3 below; TICKET-181 had it demote), so it
holds no runner slot and the slot promise holds at T=1 too. A `Kind::HostWait` native
(the `std.io` stdin readers) demotes the worker and gives its slot back for the read, direct or
inside a callback, so a runnable sibling runs while stdin is withheld, as under Go's `sysmon`.

**At `--threads=1`, a top-level `parallel:` body ALSO counts as a CPU runner (TICKET-168, W15-2,
fixed 2026-09-23).** Before this fix, a top-level body doing CPU work raced its own `chezzi-eager`
drainer — the two ran at once (196% measured), even though W8-8 had already made every OTHER T=1
shape correctly serial. The body and the drainer now share one width permit (`src/vm/width.rs`,
TICKET-141), so `--threads=1` runs at most one CPU runner including the main-thread body: fixed
measured 96% on the same shape, matching Go `GOMAXPROCS=1`'s 100%. `docs/contributing/gaps.md` **W15-9** (a
body that blocks once then burns ran n+1 runners at T>=2) is closed by TICKET-230: the body's
thread and every runner now hold one permit of the same N-wide gate at every worker count, so the
runners that outlive the unblock wait for a permit instead of adding a core. Since TICKET-205
the gate is one per process and an `Executor` job is a party of it too (see above); the body has no
gate of its own. Full tables: `docs/contributing/benchmarks.md` §TICKET-168.

## Blocking-table accounting

**The counts come from one table.** `block::wait_steps(spec, ctx)` gives each wait the counts it
registers, and `Vm::wait_begin` takes them (`Vm::nursery_counts` for a nursery join); `Vm::wait_end`
drops them before it takes the permit back (an in-place channel tick, after it, before its next
pop). A wait that ends by itself (`will_return`: a deadline or an external completion) counts
nothing, and a demoted one is `inflight`. A demoted wait that does not end by itself registers a
waiter in the ONE registry `SchedCore::waiters`. A parked fiber counts through its park (`parked_n`,
`join_parked`, a timer's `inflight`), not through the table. None of these decides a deadlock: the
run's awake count does ([The deadlock check](#the-deadlock-check)). A guard wait is never
`inflight` (DEC-063). An Executor join parks since TICKET-208 (a job is a fiber). A new blocking
shape adds a table row, never a new list.

**Cells TICKET-181 changed**, each red in the grid before the fix and judged against Go 1.27 at
`GOMAXPROCS=1` (or CPython where Go has no twin):

1. A timed `wait:` in a Demote context is `inflight` (C1, E1-E4). It faulted `deadlock` at every
   worker count; Go's `select` with `time.After` in a goroutine callback returns.
2. A socket op on a thread that owns itself blocks in a callback or `defer:` (X2). It returned
   `!"read would block: an Executor job doesn't own its thread …"`; CPython and Go block and
   read.
3. An Executor join (`ex.shutdown()`) in a fiber demotes (X1). It held the only runner and hung at
   T=1; Go and CPython complete. Since 2026-11-08 it parks instead (`ParkedEntry::ExecJoin`; the
   joined sched wakes it from `SchedCore::task_done` through `MnSched::wake_cross`): no thread per
   joiner, as in Go.
4. A `wait:` send arm on a thread that owns itself (or a job) blocks in a callback or `defer:`. It
   faulted `send on a full channel: deadlock`; Go blocks until a receiver frees the slot.
5. A `timer(ms).recv()` in a Demote context demotes. It pinned its worker, so at T=1 a sibling it
   then sent to never ran (`send on a rendezvous channel: deadlock`); Go prints the value.
6. A blocking native (`request.get`, `fs`, `process`) in a Demote context demotes (F1). It held the
   runner, so `request.get` to a sibling-served socket timed out at T=1; Go prints `[200]`.
