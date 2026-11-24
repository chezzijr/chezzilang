# Concurrency runtime internals

How the M:N engine implements the user-facing rules in
[`docs/reference/concurrency/`](../reference/concurrency/01-model.md). Read this before you change the
scheduler, the deadlock check, the width gate or `wait:`. Each rule names its ticket, so
`git log --grep TICKET-NNN` finds the commit and test that own it.

## Runner claims

One run has one scheduler, the root sched (`Vm::run_root`). Its runners are fixed at build, as Go
starts one M per P: the run's own thread is worker 0, and one `chezzi-mn` thread runs each other wid
up to `worker_count()`, all born before main's fiber is seeded (`MnSched::mark_all_runners_claimed`).
Every `parallel:` nursery and every spawned task is a fiber of that sched, so a nursery starts no
thread, and an `Executor`'s jobs run on the same workers (`QuiesceState::host`). A worker thread the
OS refuses to start gives its wid back and is not retried: the run goes on with one worker fewer
(2026-11-20). `MnSched::claim_runners` (TICKET-211) is the demand of an `Executor` sched with no host:
it claims runners while fewer than `worker_count()` run and fewer than its tasks are undone, and
`Vm::start_runners` starts them as `chezzi-exec` threads. How many runners run at once
process-wide is the one `width::RUNNERS` budget of N, main included (TICKET-230, see "One runner
budget" below). A worker whose wait demotes starts one `chezzi-mn-repl` replacement
(`Vm::demote_once`), and so does a nursery joiner that may not run fibers on its own stack
(`Vm::joiner_shell`).

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
a false fatal deadlock. A task that ends with wakes still to make (the cancel's drain after a fault, a
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

A fiber under a host frame (the Reentered context: an FFI callback, a map/set probe hook) waits in
place. It waits for the guard to come FREE holding no width permit, then takes the permit, then the
guard. Taking the guard first made 6 tasks x 2000 `update`s take 16-22 s at `--threads=2`, one 5 ms
timeout per handoff (TICKET-193). It first waits in place for up to 5 ms (`GUARD_DEMOTE_BUDGET`,
`src/vm/core.rs`), then demotes its worker: demoting on every acquire made 50 000 one-`update`
fibers exhaust a 32 768-task ceiling (TICKET-016).

## A wait inside a callback

**Every wait in user code parks its fiber, as every Go goroutine wait goes through `gopark` (plans
`2026-11-07-frame-reentry`, `2026-11-18-every-wait-parks`).** A wait in a hook, an operator (`a + b`,
`x[k]`, `-x`, `in`, `==`, `<`), an f-string part, `print`'s `str`, a list HOF callback (`map`,
`filter`, `fold`, `take_while`, `count`, `position`, `min_by`, the `sort_by_key` key pass), a sort
comparator (`sort_by`, a struct key's `compare`, `sort()` and `min`/`max` over structs), a nested
container walk, a `T.from_iter(it)` or `ch.iter()` drain, a `json.decode` `default` thunk, a
`Shared`/`RwShared` closure, a generator body or a `defer` drain parks the fiber (Park), as in Go.
The handler on the dispatch loop pushes the callee's frame with a `RetAction` (`CallFrame.ret`,
`src/vm/ret_action.rs`, `Copy`, 2 bytes) and returns; `do_return` runs the rest of the op through
`Vm::apply_ret`, and a frame that leaves without a return gives its state back through
`Vm::drop_ret`. A loop keeps its state in stack slots below the callback frame (a HOF
`[src, snap, acc, f, i]`, a drain `[src, list]`, `print` `[argc, i, line]`, a box `[box, f]`, a
generator its handle), so it needs no extra GC root and no unwind change. A sort is an iterative
merge (the same top-down split, walked with an explicit stack, so the comparator call order is
unchanged). A generator resume copies the body's one saved frame onto the caller's stacks and
copies it back at `yield`. A native callee that can wait (`xs.map(time.sleep_ms)`) runs as the step
of a VM-owned trampoline frame (`STEP_PROTO`, `src/vm/call.rs`), so its wait parks too.

**Nested walks.** A nested `==`/`!=`/`in` (`contains`, `index_of`, `unique`, `dedup`, `x.eq(y)`;
`src/vm/walk.rs`), a nested `str` (`print`, an f-string, a format spec, `x.str()`, a failed
`assert`'s operand text, a key-not-found text; `src/vm/display.rs`) and a nested `<` (`sort`,
`sort_by_key`, `min`/`max`, `x.compare(y)`; `src/vm/compare.rs`) keep their levels in operand-stack
slots, one slot group per container level. With no hooked value the walk is a tight loop that
pushes no frame. At the first hooked value it pushes the hook's frame (`RetAction::EqWalk`,
`StringifyWalk`, `CmpWalk`), and the hook's return resumes the walk. A parked nested `str` keeps
its text in one growable heap buffer, appended in place. A `json.decode` keeps its container stack
the same way and runs a `default` thunk at its field (`RetAction::JsonDefault`), as Rust serde runs
a `default`, so the output order is unchanged. The structural depth limit (`MAX_STRUCTURAL_DEPTH`,
`src/base/words.rs`) counts from `walk_base`, which is per-fiber state (`FiberCtx`, swapped by
`swap_ctx`) shared across hook nesting (W8-43); a nested `<` checks it with `==`'s text.

A box guard belongs to the task, not the worker: the task's `held_guards` and guard token travel in
its `FiberCtx` across a park. A task that holds a guard is preempted at the D3 safepoint like any
other (since 2026-11-08): a fiber that waits for its guard parks, as Go preempts a goroutine that
holds a `sync.Mutex`.

**What still runs on the Rust stack.** A push is legal only from the dispatch loop. A host frame
stays under three kinds of code (`Vm::reentered`, the Reentered context of `block::mode`): an FFI
callback (a C frame is under it, as under Go's cgo callback), a map/set probe hook (`hash`, or a
key's `eq` during a lookup or a set algebra op), and the Rust-stack `defer` drain under one of
them. A wait there demotes and waits in place, awake in the run's count, so a deadlock there hangs
(`gaps.md` W23-1). An `Inline` native or a builtin callee of a HOF or box closure (`xs.map(math.abs)`,
`ord`, `chr`, `panic`) also runs nested, because none can wait or call user code; a step frame
for it cost +63% on `xs.map(math.abs)`. `tests/owner_guards.rs`
`a_host_frame_is_counted_in_one_place` keeps the callers of `Vm::reentered`, `Vm::guarded` and
`Vm::guarded_walk` to those files. A `defer` drain runs on frames (`src/vm/defer_drain.rs`): it is a
trampoline frame that calls each record through a real call op, so a wait in a `defer` body, or in
a callback of a deferred native method (`defer xs.map(f)`), parks its task. Every hard-halt unwind
(`--timeout`, `--max-heap`, a cancel) drains through the same trampoline (`Vm::drain_then`,
`DrainCont::Unwind`), so its wait parks and its deadlock is reported. The `--timeout` deadline
still cuts every wait inside such a `defer` (TICKET-286): a `defer` that waits past it does not
finish, and no deadlock is reported. The grid is `tests/callback_park_grid.rs`: every callback kind
× `main`, a helper, top level, `spawn:` and a `test fn`, with no sender (`deadlock`, the same text
at T=1, 2, 4) and with a live sender (the golden output, `demotes=0`, no false deadlock).

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
arm, so `closed` is a ready condition on every wake predicate (park re-check, demoted wait, eager
in-place wait), the re-run poll settles on it (no spin), and the fault is gone.

**Send-arm under a host frame (TICKET-185).** A send-arm that cannot move its value under a host
frame (the Reentered context: an FFI callback, a map/set probe hook) DEMOTES and blocks on its
offer, as Go blocks — like a plain full `send` there.

**Timer arm — timed-park.** A fiber's live `timer(ms)` arm arms **one** background
`timer::submit_at(deadline, send_wake(true))` on the soonest timer arm's own channel (guarded by an
arm-once `ChannelCore.timer_armed` CAS so a re-park can't re-arm) and falls through to the normal
snapshot-park, so the timer is just another bucket. It must not sleep inline: that would pin the
OS worker and strand a sibling `send` that lands mid-window (`docs/contributing/gaps.md` **W7-14**:
`timer(300)` must not beat a value that arrives at 50 ms, as Go's `select` takes the value). The
`WaitPark` claimed-CAS sweep then picks **exactly one** of {a sibling `send`/`close` on any arm, the
timer's own deadline `send_wake`} — a value arriving before the deadline wins the wait (the value is
**not** stranded), the deadline wins only if nothing else did. Under a host frame the demote path
threads the deadline into its sleep (channel scan first, so a real send still beats the
timer).
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
  `wake_bucket`/`drain_parked`/`SchedCore::take_asleep`) CASes `claimed`, `take()`s the fiber, and
  removes its token from every other bucket by `Arc::ptr_eq` — all under the one lock, serialized with
  `park_wait`'s gap re-check (lost-wakeup-safe). Routed via `Disp::WaitPark(Vec<(key, core)>)` captured
  while the fiber heap is live (mirrors `Disp::Park`). The single-channel `recv` park stays the **1-key
  `ParkedEntry::Recv` special case** (alloc-free, provably unchanged — regression test
  `vm_wait_single_arm_recv_park_unchanged_under_parallel`).
- *Under a host frame* (`native_reentry > 0`, the Reentered context): snapshot-park is impossible, so
  `demote_wait_block` registers the wait as one `Waiter` in `SchedCore::waiters` and blocks in place.
  It sleeps on arm 0's condvar. A `send` or `close` on any other arm kicks it (`gated_register` files
  its `Wake` on each of those arms, `ChannelCore::wake_all` kicks it), so no arm is polled. At each
  wake it re-reads all N queues in source order under the core lock. The sleep ends at the sooner of
  the timer arm's deadline and the run's. The snapshot-park is the path of every other wait; the
  demote is reached only from an FFI callback or a map/set probe hook.

## A cancel is a wake

Every cancel goes through one function, `cancel(registry, CancelTarget)`
(`vm/mnsched_verdict.rs`): a child fault, an owner's abort of its nursery, `Executor.cancel()` and
its join `--timeout`, `Task.cancel()`, `os.exit`, a job's run-ending fault and the `--timeout` gap
wake. `CancelTarget` is `Scope(sched, id)`, `Flag(Option<&flag>)` or `All`. In order, it:

1. trips the flag (`trip_cancel_flag`, a `Relaxed` store). A park re-checks the flag under the core
   lock that the drain takes after the store, so a park either reads the flag or the drain finds it;
2. drains the target's parked and netpoller-parked fibers on its own sched;
3. drains the fibers under the flag on every live sched (a nested Executor keeps its jobs on a sched
   of its own);
4. pokes every live sched and kicks its demoted waits (`MnSched::wake_demoted`).

The run's `--timeout` is not a scope trip: `deadline_drain` wakes the parked fibers whose deadline
passed. A `defer` is never cut by a cancel (`Fiber::honours_cancel`); `os.exit`, `--timeout` and
`--max-heap` still cut it.

| wait | what a cancel does |
|---|---|
| a parked `recv`, `send`, `wait:`, guard or join | the drain requeues the fiber; it re-runs its op and reads the halt |
| a socket on the netpoller | the drain re-injects the fiber |
| `time.sleep_ms` in a fiber | parked as `ParkedEntry::Sleep(Arc<SleepPark>)` with one timer at the sooner of its deadline and the run's; the timer and the drain race on a claim-once flag; the halt order is `sleep_halt`. A sleeping fiber counts awake |
| a demoted `recv`, `send`, `wait:`, sleep, guard or `Executor` join | its `Waiter` holds a wake handle (`block::Wake`), and `wake_demoted` kicks it |
| a nursery join under a host frame | runs the worker loop in place; the poke wakes it |
| a demoted socket | seen within 5 ms: no condvar can wake a `poll(2)` |
| a C call, a `stdin` read | read after the call returns |

No wait polls for a cancel, except that demoted socket. Three timed waits are left, and `ticks=<n>`
in `CHEZZI_SCHED_STATS=1` counts only the first:

- a demoted socket's 5 ms `poll(2)` cap;
- a demoted guard's 50 ms `looks_free` re-check (`vm/core.rs`), which waits for a VALUE, the guard
  coming free; a cancel kicks it;
- a listed idle worker's 5 ms `idle_cv` timeout (`vm/mnsched_take.rs`, TICKET-205), which waits for a
  permit ticket, not for a cancel.

**A sleep a cancel ended stops its timer.** The party that wins a sleep's claim stops its timer
(`timer::stop`; `TimerStop` in `vm/poller.rs`, as Go's `Timer.Stop`), and the timer job holds its
sched weakly. Stopped entries leave the timer heap once they pass a quarter of it (Go 1.27's rule,
`runtime/time.go`), so the heap stays within 4/3 of its live timers. A timed `recv`, a `wait:`
timer arm and the deadline wake still keep their entry until their deadline.

**A job fault halts the run once.** The first job end that publishes a `--max-heap` fault
(`QuiesceState::request_job_fault`, the cell `JobFaultCell` in `vm/quiesce.rs`) runs `halt_run`; a
later end that carries a copy of the fault publishes and halts nothing. `Vm::rank_end` waits for that
teardown, in the GC door, before it takes the report: a teardown that landed after the report
cancelled the reap that followed it, and the same abort was reported twice. `RunHalt::Fault` stays in
force after the report until the next test's reset, so a join that wakes late still stops
(`RunHalt::ends_join`).

## One runner budget

**The budget also preempts a CPU loop under a host frame** (an FFI callback, a map/set probe hook;
`docs/contributing/gaps.md` **W14-14**, TICKET-141). That fiber cannot leave its thread — the
callback's state is on the host stack — so the THREAD hands its runner slot to one replacement worker
and takes a slot back in FIFO order (`src/vm/width.rs`, `Vm::slice_end_in_place`). `--threads=N`
stays N runners, like Go's `GOMAXPROCS=1`: a callback spin does not starve a sibling, a sleeper or
a cancelling fault. It fires only when a sibling is runnable or queued, so a lone spin costs
nothing. Every other callback runs as a frame, so its fiber yields at the slice end as any fiber
does.

**An `Executor` job is a fiber (TICKET-205, TICKET-208).** It runs on the root sched's workers and
yields at a slice end like any fiber. Before TICKET-205 a job's `Vm` skipped the safepoint: at
`CHEZZI_THREADS=1` a job in a CPU loop starved every other job, of its own executor or another,
until the loop ended (`os.exit` from a second executor's job landed after 23 s; Go at
`GOMAXPROCS=1` exits at ~100 ms). The rules, one mechanism for every kind of party:

- **One runner budget of N per process, main included: `width::RUNNERS` (TICKET-230).**
  `--threads=N` caps the threads that run Chezzi code at once at N process-wide, like Go's
  `GOMAXPROCS`, at every N. The gate holds `worker_count()` permits, read at every acquire, and it
  is the only width budget. Every thread that runs Chezzi code holds a permit while it runs: main
  takes one in `Vm::run` (`width::RunPermit`), and the root sched's workers, demote replacements and
  `Executor` runners take one in `take_runnable_inner` before they pick. Before
  TICKET-230 the gate counted runners only at `--threads=1`, so each `Executor` and each sched got
  its own N: two Executors with 4 CPU jobs each used 4.01 cores at `--threads=2`, now 1.98.
  `claim_runners` stays per-sched DEMAND (how many runners a sched wants), not width. Threads grow
  only with demotes: each demoted worker starts one replacement. The gate's state is per OS thread,
  not per `Vm` or per scheduler. A fiber under a host frame hands its permit over at a slice end
  through `Vm::slice_end_in_place`; every other fiber yields.
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
  one `DEMOTE_POLL_BACKOFF` tick: it waits for a permit ticket, not for a cancel, and `ticks` does
  not count it. A worker is listed only when fewer than two permits are free
  besides its own (`width::tight()`, TICKET-230; always true at `--threads=1`). An unlisted worker
  can receive no ticket, so it sleeps untimed on `idle_cv`.
- **Cost.** A hand-over has no CPU cost of its own, but a serialized `--threads=1` run of two CPU
  jobs costs more wall time than one sequential run when the kernel's `schedutil` governor ramps
  each thread separately: 0.90 s serial, 1.52 s with hand-overs, 0.91 s pinned to one core with
  `taskset`. A longer hand-over quantum (64 slices) measured worse (2.24 s); there is none.
  A T=1 channel hand-off costs more too: `rendezvous_pingpong` measured 1.10x pinned and 1.22x not
  pinned, accepted by the owner on 2026-10-03 (`docs/contributing/benchmarks.md` §TICKET-205).

Grid: `tests/runner_handover_grid.rs` (spinner kind x victim kind x action x worker count, 672
cells; 36 were red before the fix, all job-versus-job at one worker).
A gated thread releases its slot around every wait in place (demotes, guard waits, in-place
nursery joins and aborts, blocking natives). Since 2026-11-08 an `Executor.shutdown()` in a fiber
PARKS for the join (item 3 below; TICKET-181 had it demote), so it holds no runner slot and the
slot promise holds at T=1 too. A `Kind::HostWait` native
(the `std.io` stdin readers) demotes the worker and gives its slot back for the read, direct or
inside a callback, so a runnable sibling runs while stdin is withheld, as under Go's `sysmon`.

**At `--threads=1`, a top-level `parallel:` body ALSO counts as a CPU runner (TICKET-168, W15-2,
fixed 2026-09-23).** Before this fix, a top-level body doing CPU work raced its own `chezzi-eager`
drainer — the two ran at once (196% measured), even though W8-8 had already made every OTHER T=1
shape correctly serial. The fix made the body and the drainer share one width permit
(`src/vm/width.rs`, TICKET-141), so `--threads=1` runs at most one CPU runner including the
main-thread body: fixed measured 96% on the same shape, matching Go `GOMAXPROCS=1`'s 100%. Since
main runs as fiber 1 (2026-11-18) the body is a fiber of the root sched and there is no drainer.
`docs/contributing/gaps.md` **W15-9** (a body that blocks once then burns ran n+1 runners at T>=2)
is closed by TICKET-230: the body's thread and every runner now hold one permit of the same N-wide
gate at every worker count, so the runners that outlive the unblock wait for a permit instead of
adding a core. Since TICKET-205 the gate is one per process and an `Executor` job is a party of it
too (see above); the body has no gate of its own. Full tables: `docs/contributing/benchmarks.md`
§TICKET-168.

## Blocking-table accounting

**The counts come from one table.** `block::wait_steps(spec, ctx)` gives each wait the counts it
registers, and `Vm::wait_begin` takes them; `Vm::wait_end` drops them before it takes the permit
back (an in-place channel tick, after it, before its next pop). A wait that ends by itself (`will_return`: a deadline or an external completion) counts
nothing, and a demoted one is `inflight`. A demoted wait that does not end by itself registers a
waiter in the ONE registry `SchedCore::waiters`. An `inflight` demoted wait with a `block::Wake`
files a `Waiter` too, only to carry it (`fiber: false`). A parked fiber counts through its park (`parked_n`,
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

The table had six contexts then. Since plan `2026-11-18-every-wait-parks` (2026-11-20) it has two,
Park and Reentered (the "Demote" context above), and every cell of the shapes in 2 and 4 parks:
`main`, a `defer:` and an `Executor` job are fibers.
