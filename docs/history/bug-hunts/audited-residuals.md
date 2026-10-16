# Gaps archive: audited residuals (Tier-0 gate, wave 5) and dependency versions (moved verbatim from `docs/history/bug-hunts/gaps-archive.md` lines 4812-5498)

## Audited residuals — the Tier-0 post-merge gate (2026-07-14)

Found by the post-merge adversarial panel on the B1 merge. **Not** caused by it; none are blockers;
each is recorded rather than silently carried.

### N1. A last `print` into a just-closed pipe exits **0 or 1 nondeterministically** — **FIXED (2026-07-15)**
`stream_halt` (`src/vm/exec.rs`) is consulted **after** `emit_out` queues the line, and the EPIPE is
discovered asynchronously on the writer thread (`src/vm/stream.rs`). So for the *same* run, a program
whose final `print` lands in a pipe the reader just closed exited **0** (the VM's `Acquire` load wins →
bytes silently dropped, SUCCESS) or **1** (the writer's EPIPE lands first → `stdout closed (broken pipe)`
fault) — a ~nanosecond race decided which. Same physical outcome (a write failed, `OUT_DEAD` set), two
exit codes. **Python raises `BrokenPipeError` deterministically at write/flush.** This is the
`runtime lies to the program` family again — and it is what made `tests/interactive.rs` flake
(~1-in-N loaded; 5/60 pinned to one core). The TEST bug was fixed earlier (`read_bytes_timeout` was
manufacturing the broken pipe by dropping `ChildStdout` early, then asserting `success()` — it now drains
to EOF).

**Fix.** `flush_stream()` in `cmd_run` (`src/main.rs`) already BLOCKS on the writer ack, so immediately
after it `OUT_DEAD` is FINAL. A last print has no *next* print site, so the in-VM `stream_halt` never
fires and `errored` stays `None`; `cmd_run` now re-checks `vm::out_dead_reason()` right after the existing
`stream_error()` check and, when the VM did not already fault (`errored.is_none()`) and no `os.exit` was
requested (`exit_code.is_none()`), fails the run non-zero with the same `stdout closed (broken pipe)`
phrase. Precedence is preserved by PLACEMENT: a non-broken-pipe `stream_error` (ENOSPC, `> /dev/full`)
still wins one line above; `os.exit(code)` still outranks (the guard + the block below); a VM that
already faulted skips the check → no double-report. `out_dead_reason` was promoted `pub(super)` → `pub`
and re-exported at `src/vm/mod.rs`. The fiber path is untouched (the check runs once at process exit), so
the D5 blocking-offload invariant and two-engine parity are unaffected. Verified: pre-fix a guaranteed-drop
`print("bye") | true` exited **0** 100/100 (bug: dropped byte → SUCCESS) and `range(200) | head -1`
split 125/75; post-fix the guaranteed-drop case exits non-zero 100/100 (Python exits 120 identically),
`range(200) | head -1` is now deterministic *per physical outcome* (exit 1 ⟺ a broken-pipe diagnostic;
exit 0 only when the kernel buffer absorbed everything → nothing dropped, Python-identical), the clean
fully-drained run and `os.exit(3)`-after-print both still exit as before. Pinned by
`last_print_into_closed_pipe_is_deterministically_nonzero_{mn,serial}` +
`fully_drained_output_stays_success_{mn,serial}` (`tests/interactive.rs`).

### N2. `Socket.write`/`accept` still restart their timeout budget on every park — **FIXED (2026-07-15)**
`write`/`write_bytes` and `accept` passed `timeout.map(|t| t.deadline)` — a deadline **recomputed on
every `ip`-rewind re-execution** — exactly the budget-restart bug `Vm::poll_deadline` was added to kill
for `read`. **Fix:** extracted `Vm::socket_write` + `Vm::listener_accept` from the inline match arms and
routed their deadline through the SAME fiber latch as `read`
(`timeout.filter(|t| !t.poll_once).map(|t| *self.poll_deadline.get_or_insert(t.deadline))`), used at both
the netpoller-park and the in-callback demote sites. The extraction gives ONE `drop_poll_latch()` clear
seam per op (called from `socket_method`/`listener_method`), so the latch is set on the first park,
honored across re-parks, and cleared on completion — symmetric with `read`.

> **Note on triggerability.** Unlike `read` — which re-parks internally to finish a split codepoint — a
> `write` is architecturally **single-park**: `Socket::write` issues ONE non-blocking `write()` and
> returns `Ok(got)` on any partial success, so it only parks when the send buffer is *already full*, and
> a single park honors the deadline even with the old per-call recompute. The re-park re-arm is therefore
> only reachable on a spurious `EPOLLOUT`/`EPOLLIN` wake, not deterministically. The latch is applied for
> consistency with `read` and robustness to spurious wakes; the ordinary timeout path is pinned by
> `net_write_timeout_when_buffer_full` (a full-buffer `write` times out).

### N3. Two cosmetic B1 leftovers
- **(a) FIXED (2026-07-15).** The in-callback demote path (`src/vm/sched.rs`) and the netpoller-park path
  (`src/vm/netio.rs`) returned `Err("timeout")` even when that call already took a **partial codepoint**
  off the wire — while the poll-once path says `Err("incomplete utf-8: …")` for exactly that case, and
  `docs/reference/stdlib.md` states `Err("timeout")` means *nothing arrived*. **Fix:** a fiber-latched
  `Vm::poll_partial: Option<usize>` (the twin of `poll_deadline` — Vm + `FiberCtx` + `swap_ctx` + init +
  `drop_poll_latch` clear) is set at str `read`'s two NeedMore points (`owed` = carried byte count) and
  consulted at both timeout sites, which now report the poll-once `incomplete utf-8` classification via
  the shared `Vm::sock_incomplete_err(owed)`. `read_bytes`/`write`/`accept` never latch it, so their
  timeouts stay `"timeout"`. Tests: `net_read_timeout_bounds_the_in_callback_demote_path` +
  `net_read_timeout_bounds_whole_call_across_codepoint_parks` (flipped to assert `incomplete utf-8`) +
  `net_read_partial_timeout_then_clean_timeout_is_not_incomplete` (stale-latch clear guard).
- **(b) stays as-is (harmless, by design).** `read(0)` on a socket whose carry holds sticky **invalid**
  bytes still returns `Ok("")` (only a *closed* socket errs), so a `read(want - have)` loop that computes
  `0` cannot observe the sticky `Err`. This **matches the documented `read(0)` no-op contract**
  (`read(0)` never touches the socket and never turns a pending carry into a false EOF); surfacing the
  sticky `Err` on `read(0)` would risk that contract for no real benefit (any `read(n>0)` re-errs
  identically). Left intentionally; not a bug.

### N4. A cancelled task's `defer` **silently did not run** on M:N (spurious-deadlock race) — **FOUND + FIXED (2026-07-14)**
> **Scope correction (2026-07-14, cancellation points).** N4 fixed exactly ONE hole: an idle worker's
> spurious `Deadlocked` reaping a mid-teardown scope's parked fibers without `unwind_deferred`. It did
> **not** make "a cancelled task's `defer` always runs" true — with cancel observed at EVERY
> instruction a task could still be killed *between its first statement and its `defer` line*, so the
> defer never registered (measured: the pre-defer-`print` probe shape ran the defer in **0/20** M:N
> runs on `09cb2af`). That hole is closed by **cancellation points** (see N6 below), not by N4.

**Pre-existing** (not caused by the B1/bytes-seam merge, which touched zero lines of `src/vm/sched.rs`).
Every cancel trip and its `cancel_drain` sat **two separate core-lock acquisitions apart** — in
`mn_worker_loop` (`sched.rs`) a faulting fiber is settled by `finish(...)` and only *then* by
`cancel_drain(scope_id)`, which is what requeues the scope's **parked** siblings so they can observe the
cancel and unwind. In that window another worker's `take_runnable` evaluated `is_deadlocked`, which had
**no cancel exemption**: it saw `running == 0 && runnable == 0 && inflight == 0 && parked_n > 0 &&
done < total`, declared **DEADLOCK**, and `flag_deadlock` wrote the still-parked sibling's slot as
`Deadlocked` and **dropped the fiber without ever calling `unwind_deferred`** — so its `defer`s never ran.
A file left unclosed, a lock left held, silently.

**Why it was invisible:** `reduce_task_slots` ranks `Exit > Fault > Deadlocked`, so the *real* sibling
fault is what got reported and the spurious deadlock was completely hidden. The skipped `defer` was the
**only** symptom — no program could detect it. Same "the runtime lies to the program" family as the false
EOF (§0) and N1.

**Fix — one veto at the predicate + gapless arming at every seam that trips a cancel.** There are exactly
**three** such seams (the only scope-cancel stores in the VM): `Vm::trip_cancel` (from
`classify_mn_outcome`'s fault/exit **and**, now, `run_one_fiber`'s panic-fault fallback) followed by
`mn_worker_loop`'s `finish`→`cancel_drain`; `abort_enlisted_scope`; and `abort_eager_nursery`. (The two
demote self-detect loops only *read* a cancel — they trip none.) All three go through
`MnSched::is_deadlocked`, so the guard belongs there:

1. **The veto** — `SchedCore::any_incomplete_scope_cancelled()`: a scope with `cancel` set and
   `done < total` is *mid-teardown*, not deadlocked. Uses the **per-scope** `JoinScope::cancel`, never a
   global one — an inner fault must not veto an outer sibling.
2. **Gapless veto handoff at the two abort seams.** The veto alone was **not enough**, and the first cut
   of this fix shipped that hole: `abort_enlisted_scope` *cleared* the `awaiting_builder` veto (the one
   that had been holding the predicate off that scope) **before** it stored the cancel that arms the new
   one — a window with *neither*, in which the bug reproduced exactly as before (an idle worker's
   `flag_deadlock` dropped the parked fibers without `unwind_deferred`, and there `abort_enlisted_scope`
   discards the reduce, so not even the bogus `Deadlocked` surfaced). Both abort seams now trip the cancel
   **first** (`MnSched::trip_scope_cancel`) and only then clear their own veto (`awaiting_builder` /
   `body_open`). *An invariant enforced at the predicate is still not enforced if a seam disarms a
   different guard before arming this one* — the wave-5 lesson (§0), one level up.
3. **`trip_scope_cancel` stores under the core lock.** The bare `Relaxed` stores at the abort seams had no
   *synchronizes-with* edge to a worker holding the core lock and evaluating the predicate, so it could
   legally read a stale `false` (x86 hides this; aarch64 need not). The mutex release publishes it. On the
   fault path the edge already exists: the trip is program-ordered before `finish`, whose lock release the
   predicate's `running == 0` depends on.
4. **The panic-fault path now trips the cancel** (`Vm::panic_outcome`). A worker-VM panic (a VM bug, a
   panicking native/FFI callback) never reaches `classify_mn_outcome`, so the scope aborted with
   `cancel == false`: `cancel_drain` requeued the parked siblings, they re-ran `recv`, `park`'s gap
   re-check saw no cancel and **parked them again**, and the scope then quiesced *uncancelled* → a
   deadlock that fired **by the predicate's own rules** → same dropped `defer`s, hidden behind the
   panic-fault (Fault > Deadlocked).
5. **The netpoller park is gated on the per-scope cancel.** `poller::register` read the sched-level
   (= OUTERMOST nursery's) flag, so a fiber of a cancelled **inner** scope could park on a poller whose
   `drain_sched` sweep had already run — stranding it, and (with the new veto) holding the veto **forever**
   → deadlock detection disabled sched-wide. `poll_park_offload` now hands `register` the parking fiber's
   `scopes[fiber.scope_id].cancel`, exactly like `park`/`park_wait`'s gap re-check. The now-dead sched-level
   `MnSched::cancel` field is **deleted** (its last reader).

**Liveness** (why the veto can never become a hang): every park path refuses to park a cancelled-scope
fiber (`park`/`park_wait` requeue `Ready` under the core lock; `register` rejects under the registry lock
`drain_sched` sweeps under — see (5)), every trip is followed by a `cancel_drain` that requeues + notifies,
and both demote self-detect loops check their cancel before the deadlock check. So a cancelled scope always
drains to `done == total` and the veto is **transient by construction**. Genuine deadlock detection (nothing
cancelled anywhere) is untouched — `mnsched_deadlock_when_all_parked_runq_empty` still passes.

Repro was a **race**: `parallel_defer_runs_on_cancelled_sibling` printed `0` instead of `42` in
**14/200** runs under CPU contention on the fix box (**35/200** in an earlier run on a busier one) before the fix, **0/200** after (and `--threads=1`/`2` always passed —
no idle worker, so the window could not open). **Re-derived 2026-08-18 (`W8-8` closed): the stated reason
holds in both epochs, but for different reasons.** This repro spawns exactly TWO tasks, so pre-fix — when
`--threads=1` was secretly two-wide — worker count happened to equal task count and there was coincidentally
no third, idle worker; post-fix there is structurally only one worker at `--threads=1`, so nothing can race.
`--threads=2` was never the special case and is untouched. Re-measured on the post-fix binary, 50 runs at
each width: **50/50 printed `42` at both, 0 timeouts, 0 wrong values**. The `abort_enlisted_scope` seam has its own scenario test
(`parallel_defer_runs_when_enlisted_nursery_escapes`, an early-enlisted outer nursery escaped by `return`);
with a 30ms sleep probing the veto-free gap it printed `cleanup=0` **20/20** on the old ordering and `42`
**20/20** on the new one. The invariants themselves are pinned by
`mnsched_cancelled_scope_with_parked_fibers_is_not_deadlock` (the predicate),
`panic_fault_trips_the_scope_cancel` (4) and `poll_park_rejects_cancelled_inner_scope` (5), which assert the
rules directly rather than a scenario. `reduce_task_slots`'s ranking is **not** touched: it is correct — the
spurious `Deadlocked` simply must never be produced.

### N8. `--serial` HANGS on a CPU-bound sibling — cooperative engine never preempts it — DOCUMENTED KNOWN-LIMIT (won't-fix 2026-07-15; use `--threads=1`)
Found 2026-07-15 by the post-merge harness; **pre-existing** (reproduces identically on `09cb2af`, before the
cancellation-points work). A `parallel:` with one task in a long CPU loop and one that faults:

| engine | result |
|---|---|
| M:N (default) | the spinner is cancelled promptly at its back-edge checkpoint |
| `--serial` | **HANGS** — the spinner never yields, so the sibling never runs, never faults, and the cancel that would kill the spinner is never tripped |

Cancellation points put a checkpoint on every loop back-edge, but a checkpoint only *delivers* a cancel that
someone already tripped. On the cooperative engine nothing can trip it while the spinner holds the thread.
The serial scheduler *could* be taught to preempt (the `reds` reduction counter already exists — D3, but is
gated `if self.mn.is_some()` at `src/vm/exec.rs:858` and the cooperative scheduler has no time-slice path for
a *running* fiber — a rearchitecture, its own milestone).

**DECISION (2026-07-15): won't-fix — documented known-limit.** `--serial` is only the byte-identical parity
**oracle** for bug-finding, never the recommended user runtime; **`--threads=1`** already gives safe
single-thread execution (OS-thread M:N — the kernel preempts the spinner, verified 0/15 hangs), which makes
a cooperative time-slicer unnecessary for users. **RE-DERIVED 2026-08-18 (`W8-8` closed).** When this
decision was taken `--threads=1` silently ran TWO CPU runners, so "single-thread" was aspirational and the
spinner had a whole second worker to be preempted *around*. It is now genuinely one runner, and the repro
was re-run 15× on that binary: **15/15 rc=1 in 4–6 ms with `division by zero`, 0 hangs**, the spinner
never reaching its 2e9 bound. `CONTEXT_REDS = 4000` reduction-budget preemption (`src/vm/exec.rs:1160-1166`)
fires per dispatched op regardless of worker count, so one runner suffices. The won't-fix stands on a
measurement that now means what it says. Recorded in `docs/reference/concurrency.md` §"Cooperative contract
(by design)". Reopen only if `--serial` ever ships as a user-facing runtime.

### N9. A cancelled task's OUTPUT LINE SET differs between engines — inherent — DOCUMENTED KNOWN-LIMIT (won't-fix 2026-07-15; same root as N8)
Same shape as N8 and also **pre-existing** (`09cb2af`: M:N emits 1 line, sometimes 0; serial emits 5). A task
cancelled mid-loop emits *however far it got*, and "how far it got" is a scheduling fact:

| engine | lines a cancelled 5-iteration loop emits |
|---|---|
| M:N | 1 (it is cancelled at its first back-edge after the sibling faults) |
| `--serial` | 5 (it runs to completion **before** the sibling ever gets a turn to fault — see N8) |

This is not an ordering question (the docs already declare cross-task print ORDER nondeterministic) — it is the
line **set**. It is a real serial ≠ M:N divergence and the parity oracle cannot see it, because no parity test
has this shape. Fixing N8 (serial preemption) would largely close it: once serial yields at back-edges, the
cancelled task dies at a back-edge on both engines. **The cancellation-points work made M:N side of this
DETERMINISTIC (always 1, never 0) — it did not create the gap.**

### ~~N10.~~ A `wait:` timer arm makes `--serial` inline-sleep instead of yielding to a runnable sibling — **CLOSED 2026-08-16 FOR THE COOPERATIVE FIBER** (found 2026-07-22)

> **What closed it: the cooperative fiber no longer exists.** The whole cooperative scheduler — and with
> it the **cooperative multi-channel park** that sat *below* the inline-sleep block — was deleted with
> `--serial` (`89f20591`; `docs/design/roadmap.md` §2b). Re-measured on the post-removal binary, the repro below
> prints `recv 42` in ~6 ms, **5/5**.
>
> **Read that as narrower than it sounds. Do NOT read it as "a `wait:` timer arm can no longer swallow a
> ready sibling."** The `op_wait_poll` **inline-sleep block itself was deliberately KEPT**. It was never
> serial-only: it is still reachable on M:N by the **inline outermost-`parallel:` builder** — `mn ==
> None` with `mn_enlist_sched == Some` (`src/vm/sched.rs:527`), a body that owns no worker loop to drive
> a park — and **that surviving arm still sleeps to the soonest deadline and takes the timer arm without
> re-polling siblings**. Keeping it is correct there: the only alternative on that path is the
> all-parties-blocked fault, which would be a wrong answer. So the *shape* this entry describes persists
> in one live context; what is gone is the engine on which it was the default path. The code says the
> same thing at `src/vm/netio.rs`, above that `if let Some((i, deadline)) = soonest` arm.

The original finding, kept as the record. Found 2026-07-22 by the bug-hunt (channel/wait domain). A `wait:` with a live `timer(ms)` arm **and** a
runnable sibling that can satisfy a non-timer arm diverges between engines. Deterministic (the sibling
sends with zero delay, the timer is 5 s → the recv must always win, per Go `select`), so it is a **wrong
result**, not a timing race:

```chezzi
import std.time
fn main():
    data := Channel[int]()
    parallel:
        spawn:
            wait:
                v := data.recv(): print("recv {v}")
                _ := timer(5000).recv(): print("timeout")
        spawn:
            data.send(42)
main()
```

| engine | result |
|---|---|
| M:N (default) | `recv 42` in ~3 ms — the ready sibling send beats the 5 s timer (correct, Go model) |
| `--serial` | **`timeout`** after inline-sleeping the full **5.004 s** — the timer arm is taken, the send is stranded |

`chezzi run --check-parity` reports `parity DIVERGENCE (serial != M:N)`.

**Root cause** — `src/vm/netio.rs` `op_wait_poll`, the cooperative serial branch (~line 1798). The live-timer
**inline-sleep** block sits *before* the cooperative multi-channel park block, so a `wait:` with a timer arm
inline-sleeps even when `scheduler_stack` has a runnable sibling — it never yields. The M:N path already got
the fix (the **WAIT-1** comment ~line 1735: arm one background `timer::submit_at(deadline, send_wake)` and
fall through to snapshot-park, so a real send lands first and the timer is just another bucket); the serial
path was never given the equivalent. **Distinct from N8** — the sibling here is a cooperatively-schedulable
blocking `send`, not a CPU-bound busy-loop, so there is no preemption barrier; it is cleanly fixable (park on
the channel arms first, make the cooperative quiesce path deadline-aware so it inline-sleeps the timer only
when it would otherwise idle-deadlock).

**DECISION (2026-07-22): pre-freeze known-limit; fix deferred to the serial removal.** *(That removal
shipped 2026-08-16 — and PRE-freeze, not post; see the verdict block at the top of this section.)* The
shipping **M:N engine is correct** — user-facing impact is zero. Fixing it is a real cooperative-scheduler
change right before the JIT freeze, and the serial engine is slated for **removal** post-freeze anyway (the
oracle-layer plan in `docs/design/roadmap.md` §2b). So the byte-identity tax that motivated a fix is going away.
Falsified doc claims corrected in the same commit: `docs/reference/concurrency.md` "Observable output is identical
across both engines" (the `timer` note) and the two "serial == M:N byte-identical" `wait:` claims — each now
points here. Excluded from the bug-hunt harness like N8/N9 (a `wait:`-with-timer-and-runnable-sibling shape is
a known serial≠M:N divergence — don't re-file).

**Update 2026-08-05 (W7-17), N10 itself unchanged.** That inline-sleep was a bare
`thread::sleep(deadline - now)` — the one W7-16 missed — so `chezzi test --timeout` could not reach a
serial `wait:` timer arm either (measured 3004 ms, post-wait statement ran). It is now
`block_until_deadline`, i.e. the same sleep observed in `DEMOTE_POLL_BACKOFF` chunks. It still sleeps to
the deadline and still takes the timer arm without yielding to a runnable sibling — the divergence this
entry describes is exactly as before; it just observes the halts on the way.

### N5. A **genuine** deadlock tears tasks down without running their `defer`s — **CLOSED 2026-08-06, NOT A BUG: every ancestor does the same (or worse)**

**The premise was a mis-paired comparison, and that is the whole entry.** The filing below reasoned
"arguably the same silent-lie class as N4 — **Go still runs deferred fns on a panic**". True, and
irrelevant: a Chezzi deadlock is not Go's `panic`, it is Go's **fatal error**, and those two paths
differ *precisely* on whether deferred work runs. Measured, same three-line shape in each runtime — a
receive on a channel nothing will ever fill, with cleanup registered:

| runtime | what happens | cleanup runs? |
|---|---|---|
| **Chezzi** (`defer`, both engines) | faults `deadlock: every task in this parallel: block is blocked…` in ms | **no** |
| **Go** (`defer`, `<-ch`) | `fatal error: all goroutines are asleep - deadlock!` | **no** |
| **CPython** (`finally`, `queue.Queue().get()`) | **hangs forever** — killed by an external `timeout 6` | **no** |
| **CPython** (`finally`, asyncio `TaskGroup` + unset `Event`) | **hangs forever** — killed at 6 s | **no** |
| *control:* **Go** `defer` + `panic("boom")` | `panic: boom` | **yes** — `DEFER RAN` printed first |

Chezzi is the **strictest** of the three: it detects and reports in milliseconds where CPython hangs
until something kills it, and it declines to run cleanup on exactly the path Go declines to. There is
no ancestor to converge on, so there is nothing to fix. Both engines agreeing was never the argument
(that is a detector, not a standard) — the ancestors are.

The original filing follows; its two engineering reasons are still accurate, they were just answering
a question that turned out not to be open.

Found while fixing N4, and **independent** of it. `flag_deadlock` (`src/vm/mod.rs`) drops each parked
`Fiber` **without** `unwind_deferred`, so on a real deadlock (every fiber parked, nothing cancelled, no
send possible) the tasks' `defer`s are skipped. Arguably the same silent-lie class as N4 — Go still runs
deferred fns on a panic. Deliberately **not** folded into the N4 fix, for two reasons:
1. The **serial** oracle does the same (it faults from the parent nursery join and never resumes the
   parked children), so the two engines currently **agree**. Fixing M:N alone would *break* serial == M:N
   parity — this is an engine-consistent **known limit**, not a divergence.
2. `flag_deadlock` runs inside `SchedCore` under the core lock with no `Vm` shell, so it cannot execute
   bytecode there. A real fix means requeueing the parked fibers with a deadlock sentinel (plus a matching
   serial change), which moves deadlock-path stdout ordering — a behavior change, so its own task.

Documented as the one exception to the "cancellation always runs `defer`" guarantee in
`docs/reference/concurrency.md` — and that exception **stays**, now as a stated contract rather than a filed
debt: a deadlock is the runtime declaring the program cannot proceed, which is not a cancellation.

### N6. `--serial` abandoned a PARKED task's `defer` on a sibling fault — **FIXED (2026-07-14, `auto-task/cancel-points`)**
Found while verifying the N4 fix end-to-end on the CLI (**not** caused by it — reproduced on unfixed
`main`, `0b23703`). Serial's `run_scheduler` (`src/vm/sched.rs`) drove children with `run_child(i)?` —
the `?` propagated the faulting child's error **straight out of the scheduler loop**, so the still-parked
children were abandoned where they sat: never resumed, never cancelled, never unwound. On the
token-sequenced repro (defer GUARANTEED registered) M:N printed `42` and serial printed **`0`, 10/10**.

**Fix (two independent changes; the language-semantics one is BUG 1 below).**
1. **Serial cancel drain** — `run_scheduler` now saves/restores the enclosing scope's cancel state around
   each level (`run_scheduler_level`), and on a child fault/exit trips a transient scope cancel and
   **re-drives every still-`Blocked` sibling** (`drain_cancelled_children`, task order) so each observes
   the cancel at its rewound park op and unwinds its `defer`s **before** the fault propagates. Exits are
   reduced exactly like M:N's `reduce_task_slots` (**`Exit` > `Fault`, lowest task index wins**), so an
   `os.exit` executed by a *drained child's `defer`* is carried, never discarded. A cancelled task's
   already-printed bytes are **kept** (serial prints live and cannot un-print).
2. **Cancellation points (BUG 1, BOTH engines)** — cancel is no longer observed at every instruction (the
   every-instruction check at `run_until`'s loop top is **deleted**). It is delivered at **checkpoints**:
   **loop back-edges** (`Vm::jump_checked` — a backward `Op::Jump`, pinned by
   `compiler::back_edge_tests::loop_back_edge_is_a_backward_jump`) and **blocking/park ops**
   (`chan_recv_step`, `op_wait_poll`, `park_on_fd`, the blocking-native offload — each now an
   engine-agnostic top-of-fn check, replacing the `mn`-gated ones) and **native→user-code re-entries**
   (`Vm::guarded` — a native HOF's per-element callback is that Rust loop's back-edge; see N6c).
   Consequences, all intended:
   a **started task always runs its straight-line prologue**, so a **registered `defer` always runs on
   cancel — on both engines**, deterministically (the old behavior made "does my cleanup run?" a
   scheduler race: 0/20 on the probe shape); a CPU loop is still promptly cancellable (the back-edge);
   at a `recv`/`wait:` checkpoint **cancel now wins over a queued value / a tripped done-latch / a fired
   timer**, uniformly on both engines. Cost, accepted: cancellation is less prompt — a cancelled task
   runs to its next checkpoint. This is Trio's model; the old every-instruction kill was neither Go's
   (goroutines are never preemptively killed) nor Trio's.
3. **M:N `TaskOutcome::Cancelled` now carries its output** and flushes it at its task-order slot
   (`classify_mn_outcome` / `reduce_task_slots`): with (2) a cancelled task really did print those lines,
   and serial cannot un-print them — dropping them was a capture-mode-only line-SET divergence.

**Output-order rule (documented, not a bug):** cross-task stdout ORDER is nondeterministic on **both**
engines (one `print` = one locked, line-atomic write) and is **not** part of the parity contract. What is
identical across engines: the **line set**, the **exit code**, and **whether the `defer` ran**. Parity
tests of concurrent output use `assert_same_lines`.

**Evidence (release binary, N-1 CPU load generators, 200 runs per engine per shape — 0 failures each):**
`defer`-first immediate-fault, the **probe** shape (a `print` BEFORE the `defer`) and the
token-sequenced shape all print `42` on M:N and `--serial`, **0/200** failures per engine per shape
(before: probe = defer ran in **0/20** M:N runs; token = serial `0` **10/10**). Parity tests:
`parity_defer_runs_on_parked_sibling_when_sibling_faults`,
`parity_probe_defer_runs_when_cancelled_before_its_defer_line`,
`parity_os_exit_inside_a_cancelled_tasks_defer` (+ `parallel_spinning_sibling_does_not_hang_the_nursery_under_cancel`
for a `while true:` sibling). None of these shapes had parity coverage before — which is exactly why a
live divergence survived ~1500 green tests.

### N6b. EVERY spawned task starts — including into an already-cancelled scope (adversarial-review fix)
The first cut of the N6 drain re-drove only **`Blocked`** siblings and deliberately skipped never-started
(`Pending`) ones, on the theory that M:N merely *races* them. It does not: M:N is **structurally forced**
to start every spawned fiber — a scope completes only at `done == total` and `take_runnable`
(`src/vm/mod.rs`) never consults the scope cancel — so a queued fiber is popped and started *after* the
cancel trips, and with cancellation points it then runs its whole straight-line prologue. Measured on the
first cut (`spawn boom(); spawn talker(ch, s)`, faulter FIRST): **serial `{"0"}` vs M:N `{"hi","42"}`,
20/20** — a deterministic line-SET *and* defer-ran divergence, i.e. exactly the parity contract this
change declares. (The old every-instruction check had hidden it: the freshly-started M:N fiber died at
its first dispatched op and its output was dropped.)

**Fix:** `drain_cancelled_children` drives **every not-`Done` sibling**, `Pending` included, with the
cancel tripped. Both engines now start every spawned task, run its prologue, run any `defer` it
registers, and agree on the line set. `exit_in_spawned_child_aborts_siblings`'s serial golden moved
deliberately (`{"a"}` → `{"a","b"}`): M:N already printed `"a","b"` **20/20**, so this is the two engines
converging, not a regression — `os.exit` is a hard halt for the *program* (reduced at the nursery join),
not a freeze-frame on tasks the nursery already spawned.

### N6c. NO cancellation point in a native-driven loop — FIXED; in loop-free RECURSION — accepted limit
Two long-running CPU shapes have **no backward `Op::Jump`**, so the back-edge checkpoint cannot see them:

1. **Native-driven user code — FIXED.** `list.map`/`filter`/`fold`, `sort(cmp)`, an operator overload, an
   `Executor` handler all iterate in **Rust** (`for e in .. { self.guarded(|vm| vm.invoke_value(f, ..)) }`,
   `src/vm/call.rs`) and emit no `Op::Jump`; a straight-line callback body has no back-edge either. The
   first cut of this change therefore let a cancelled task burn every remaining element to completion,
   with its prints / `Shared` writes / fs writes (measured: `xs.map(sq)` over 5M elements ran to
   `"map finished"` long after the sibling had faulted; the deleted every-instruction check used to abort
   it via the `?` on `guarded`). **A native HOF's per-element re-entry IS that loop's back-edge**, so the
   cancellation checkpoint now lives at the top of `Vm::guarded` (`src/vm/exec.rs`) — one choke point, no
   new hot-path cost (it only reads the flag when re-entering user code from native). Test:
   `parity_native_hof_loop_is_cancellable`.
2. **Loop-free recursion — ACCEPTED LIMIT, both engines.** A recursive function emits only `Call`/`Return`
   (the repo's own `fib` bench), so a cancelled task inside one runs the whole computation before it dies
   (measured: `fib(32)` completes and prints after the sibling faults). Making `Op::Call` a checkpoint is
   **rejected**: it would put a cancellation point *before the `defer` line* of any prologue that calls a
   function — precisely BUG 1, back again. Pure-CPU code being uninterruptible is Trio's model, both
   engines agree, and `MAX_CALL_DEPTH` bounds the stack (not the time). Bound the recursion yourself if a
   task must tear down promptly.

### N6d. A `defer` was itself cancelled — the LIFO-first defer was SILENTLY SWALLOWED (adversarial-review fix, round 2)
The first cut of the cancellation-point change put a checkpoint at the top of `Vm::guarded` (N6c) —
and **every deferred call runs through `guarded`** (`run_one_deferred`, `src/vm/call.rs`). A task that
ends on the **normal-return** path (or faults on its own) while a sibling has already tripped the scope
cancel has `self.cancelled == false`, so that checkpoint fired on the FIRST (LIFO) deferred call and
returned `cancelled` **before its body ran**; only the remaining defers executed. Arbitrary **partial
cleanup** — one fd released, the next not. Deterministic, on BOTH engines (so parity stayed green: both
dropped it identically). Repro: `parallel: { spawn boom(); spawn tidy() }` with
`fn tidy(): defer print("cleanup1"); defer print("cleanup2"); print("start")` → `start`, `cleanup1`, and
`cleanup2` **never printed**. The same hole applied to a loop (`jump_checked`) or a blocking op inside a
defer body.
**Fix:** a `defer` is the cleanup the cancel exists to run, so **no cancellation point fires inside a
deferred call**. `Vm::deferring` (a depth counter raised in `run_one_deferred`) is read by the ONE cancel
predicate every checkpoint now calls — `Vm::cancel_requested` (`src/vm/exec.rs`), which also keeps the old
`!self.cancelled` unwind latch. Test: `parity_every_defer_of_a_normally_returning_task_runs_under_a_tripped_cancel`.

### N6e. A nested `parallel:` inside a cancelled task was UNCANCELLABLE — the teardown HUNG (adversarial-review fix, round 2)
Structured concurrency says cancelling a scope cancels its **descendant** scopes. It did not: a nested
nursery got a fresh cancel flag (M:N `register_scope`) / serial handed the level `cancel = None`, so the
nested children's back-edge checkpoints had **no tripped flag to read** — a spinning grandchild looped
forever and the whole teardown never finished. **NEW HANG on both engines** (measured with `timeout`;
`main` did not hang only because the deleted every-instruction check killed the parent fiber before it
could enter the nested nursery — a timing accident).
**Fix:** a nested scope keeps its OWN `cancel` (an inner fault must never cancel an outer sibling — the
other half of the invariant) and additionally inherits its enclosing scopes' flags: `JoinScope::ancestors`
→ re-pointed per fiber swap-in into `Vm::cancel_outer`, read by `cancel_requested`. Serial's
`run_scheduler` inherits the enclosing `Arc` directly (and hands a **clean slate** to a nursery started
from inside a `defer` — that cleanup must run). Test:
`parity_nested_nursery_inside_a_cancelled_task_is_cancellable`.

### N6f. The blocking-op checkpoint existed only on M:N — serial ran a cancelled task PAST it (adversarial-review fix, round 2)
The blocking-native cancel check was written INSIDE the `self.mn.is_some()` offload gate
(`src/vm/call.rs`), so `--serial` had **no** cancel-delivery point at `sleep_ms` / `io.*` / `fs.*` /
`request` / `process` at all. With the every-instruction check gone, a cancelled serial task ran the
blocking call to completion (stalling the entire teardown for its full duration — `sleep_ms(60000)` would
freeze it for a minute) and then, having no further checkpoint, ran **every straight-line statement after
it**. Deterministic line-SET divergence: `{napper start, napper woke, end}` on serial vs
`{napper start, end}` on M:N; with an `os.exit(7)` after the sleep, the exit CODE diverged too (7 vs 1).
**Fix:** the check moved OUTSIDE the `mn` gate (and the same for `park_on_fd`'s socket checkpoint), so the
cancellation-point SET is engine-agnostic, exactly as the contract claims. Test:
`parity_blocking_native_is_a_cancellation_checkpoint_on_both_engines` (also asserts the teardown does not
wait out the cancelled task's 3 s sleep).

### N6g. A `defer` that BLOCKS: truncated mid-body on M:N (fixed), and — if it can never complete — a silent M:N HANG (fixed)
Two bugs at the same seam, both found by running a cancelled task's `defer` through a *blocking* body —
which is what real cleanup does (close a socket, send a final message, flush). Both were introduced by
this branch's own rules (a `defer` is not itself cancellable, N6d) and both are M:N-only, i.e. live
serial != M:N divergences.

1. **Cleanup TRUNCATED mid-body (M:N).** The M:N demote paths (`demote_recv_block`, `demote_block_sleep`,
   `src/vm/sched.rs`) read the raw `self.cancel` flag instead of the `Vm::cancel_requested()` predicate.
   A defer body runs under `guarded` (`native_reentry > 0`), so a blocking op inside cleanup demotes and
   lands there — and the raw read fires on the already-tripped scope flag, aborting the defer *at that
   call*: `CLEANUP-ENTER` and then nothing, sentinel `0` on M:N vs `42` on serial (which runs the same
   call inline). The predicate's `deferring == 0` term is exactly what keeps cleanup atomic, and it also
   folds in `cancel_outer` (an *enclosing* scope's cancel), which a raw read misses. **Fixed** by routing
   both demote loops through `cancel_requested()`. Guard:
   `parity_a_blocking_defer_body_completes_when_the_task_is_cancelled`.
2. **Cleanup that can NEVER complete → SILENT M:N HANG (the N4 veto never lifted).** A `defer` whose body
   `recv`s on a channel nobody will ever send to correctly cannot be cancelled out — so on M:N it sits in
   `demote_recv_block` forever. That loop *does* self-detect deadlock every backoff cycle
   (`sched.is_deadlocked`), but the predicate was vetoed by **N4's** `any_incomplete_scope_cancelled` —
   "some incomplete cancelled scope" — and the scope is incomplete *precisely because* that fiber is stuck
   in its own cleanup. The veto's own liveness argument ("a cancelled scope always reaches
   `done == total`, so the veto is transient by construction") is falsified by a never-completing defer.
   Measured: M:N `timeout` rc=124 (prints `CLEANUP-ENTER`, then hangs forever), serial rc=1 (reports the
   sibling's real fault). **Fix:** bound the veto to the window it exists for — the trip→`cancel_drain`
   gap — by asking for an **undrained PARKED fiber** of the cancelled scope
   (`SchedCore::any_cancelled_scope_awaiting_drain` + `scope_has_undrained_park`, `src/vm/mod.rs`, which
   scans `parked` exactly as `cancel_drain` does, under the same core lock). Once drained those fibers are
   in `global` → `runnable > 0` → the predicate is false on its own terms, so the veto is not needed past
   that point; and a cancelled scope cannot re-accumulate parked fibers (every park path re-checks its
   scope's cancel). The netpoller half of the drain window needs no veto: a poll-parked fiber is not in
   `parked` and is accounted `inflight`, which `is_deadlocked` already requires to be 0. Post-fix, the
   quiesce fires, the demoted fiber faults in place, its error is swallowed (its task is cancelled) and
   the **sibling's real fault** is reported — the same line set serial prints. Predicate tests:
   `mnsched_cancelled_scope_whose_only_fiber_is_demoted_is_deadlock` (fires) and
   `mnsched_cancelled_scope_with_a_parked_and_a_demoted_fiber_is_not_deadlock` +
   `mnsched_cancelled_scope_with_parked_fibers_is_not_deadlock` (still vetoed — N4 intact). Parity test:
   `parity_a_defer_that_can_never_complete_is_reported_not_hung` (hard 20s deadline: a hang fails the test
   instead of wedging the suite).
3. **…and the bounded veto lost the DEMOTED half (adversarial-review round 4, fixed before merge).**
   `parked`-only was too narrow the other way: a fiber demoted (`blocked_native`) in its **body** —
   a `recv` reached inside a native HOF callback / `Shared.update` / an `Executor` handler — is *not* in
   `parked`, yet a cancel WILL wake it (`demote_recv_block` ranks `cancel_requested()` above `terminate`
   and above its own self-detect), whereupon it unwinds and runs its `defer`s, which can `send`. **CANCEL
   is a wakeup source the `running`/`runnable`/`inflight`/`parked` counters do not model**, so with a
   cancelled scope whose only unsettled fiber was demoted, an idle worker could declare a spurious
   deadlock in the ≤5 ms `DEMOTE_POLL_BACKOFF` window before that fiber noticed the cancel — and
   `flag_deadlock` then reaps every parked fiber of **every** scope without `unwind_deferred` (the exact
   N4 lost-defer symptom) and latches `terminate`, truncating any sibling that is demoted inside its own
   `defer`. **Fix:** each demoted fiber now WATCHES the cancel flags it would honour
   (`Vm::demote_cancel_flags` → `SchedCore::watch_demoted_cancel`, dropped on every demote-loop exit);
   `is_deadlocked` vetoes while any watched flag is tripped (`any_demoted_cancel_pending`). The watch is
   EMPTY when a cancel could not wake the fiber anyway — already unwinding (`cancelled`) or blocked
   inside its own `defer` (`deferring > 0`; neither term can change while it is blocked in place) — which
   is precisely the never-completing cleanup of (2), so that still fires as a genuine deadlock. The veto
   is self-lifting (the entry disappears when the fiber settles) and is now evaluated *after* the counter
   gate, i.e. only at a candidate quiesce, so the `parked` scan is off the idle/steal hot path. Predicate
   test: `mnsched_demoted_fiber_with_a_tripped_cancel_is_not_deadlock` (RED before the fix).

4. **N6h — a nursery opened INSIDE a cleanup `defer` had its children cancelled (M:N only).** The
   `deferring > 0` suppression that makes a defer uncancellable is **per-`Vm`** and does not cross the
   airlock: a worker fiber is a fresh `Vm` with `deferring == 0`. The cancel-flag CHAIN does cross it
   (`Vm::scope_ancestors` → `JoinScope::ancestors` → `cancel_outer`), so a task spawned by a cancelled
   task's cleanup inherited the already-tripped enclosing flag and died at its first checkpoint —
   silently, rc 0 (`CLEANUP-ENTER|CLEANUP-DONE|sentinel=0` on M:N vs `sentinel=42` on `--serial`,
   deterministic; `main` agreed with serial, so it was a REGRESSION introduced by the N6 fixes above).
   Serial severs the enclosing cancel in a defer (`run_scheduler`'s `in_defer` → `self.cancel.take()`);
   **fix:** `Vm::scope_ancestors` severs identically (empty chain while `deferring > 0`), so the defer's
   own nursery gets a clean slate (and still its own fresh flag for its own faults). Test:
   `parity_a_nursery_inside_a_cancelled_tasks_defer_runs_to_completion` (RED before the fix).

**THE RULE (both engines, now documented in `docs/reference/concurrency.md`):** a `defer` is never itself cancelled
and runs to completion, blocking ops (and the work it *spawns*) included. Cleanup that blocks on
**time/IO** is uninterruptible and delays the teardown for exactly as long as it takes — no cap
(`defer time.sleep_ms(10000)` in a cancelled task = a 10 s nursery join, on both engines). That is Go's
rule for a deferred fn during a panic, and it is a documented ceiling, not a bug — a cap would
re-introduce silent truncation. Cleanup that can **never** complete is REPORTED as a deadlock, never a
silent hang. **One carve-out (C5, below): on `--serial` a defer body cannot PARK.**

### ~~N6g~~ — **DISSOLVED 2026-08-16** (C5 family): a `defer` that `recv`s from a LIVE sibling cannot park on `--serial`

> **Nothing was fixed — the engine that could not park was removed** (`89f20591`; `docs/design/roadmap.md` §2b).
> The only behaviour left is M:N's, which was always the correct one: the `recv` **demotes** (the worker
> blocks in place on its own thread, a replacement is spun) and the sibling's `send` reaches it, so the
> cleanup completes. The pinning test
> `c5_limit_a_defer_that_recvs_from_a_live_sibling_cannot_park_on_serial` was deleted with it
> (`1de5d5c0`) — its whole subject was the serial-only limitation, and once `run_file`'s serial leg
> collapsed onto M:N its own `serial`-labelled variable silently became a second M:N run.
>
> **The underlying C5 constraint is NOT dissolved.** A `defer` body still runs *guarded* and still
> cannot snapshot-park; lifting that needs C5 (a resumable native re-entry / a VM-driven defer drain).
> What died is the *divergence*, not the *limitation*.

The original finding, kept as the record. A defer body runs `guarded` (the LIFO unwind drain is host-stack state), so — exactly like a `list.map`
callback — it cannot snapshot-park on the cooperative engine. A `recv` inside a cleanup whose value a
live sibling *will* send therefore cannot yield to that sibling on `--serial`: it faults **in place**
with the C5 deadlock error. On M:N the same recv DEMOTES (blocks in place on a real thread) and
completes. Two measured shapes, both pinned by
`c5_limit_a_defer_that_recvs_from_a_live_sibling_cannot_park_on_serial`:
- **no cancellation at all** (pre-existing on `main`, unchanged by this branch): serial prints the C5
  deadlock error at the recv site and the cleanup stops there; M:N completes the cleanup. This is an
  **outcome-level** divergence (different line set), *not* the "message-only" one previously recorded
  here — that characterisation was wrong and is corrected.
- **a cancelled task** (new surface — before the N6 fixes serial ran no defer at all here): the in-place
  fault is *swallowed* with the cancelled task, so serial's cleanup simply stops at the recv while M:N
  finishes it.
Lifting it needs **C5** (a resumable native re-entry / a VM-driven defer drain), not a cancellation
change; faulting M:N's demoted recv to "match" would trade a real capability for a tidier oracle. Cleanup
that sends, sleeps, closes or computes is unaffected — the park is the only thing serial cannot do.

**Out of scope, measured, recorded (no hang, no lost cleanup — do not "fix" one engine alone):**
- A fiber already **PARKED inside a NESTED nursery** when the *outer* scope is cancelled does **not** run
  its `defer`s — on **either** engine (measured 3/3 each: `sentinel=0` on M:N and on `--serial`; `main`
  agrees). The cancel drain is scope-scoped and a parked fiber has no checkpoint at which to observe the
  inherited `cancel_outer` flag, so the fiber is reaped by the deadlock teardown instead (M:N
  `flag_deadlock`; serial's nested `run_scheduler_level` `None` arm — it cannot switch back to the outer
  level to run the faulting sibling at all). This is the **N5** family, not a cancel bug: both engines
  agree, so parity holds, and draining descendant scopes on M:N alone would *create* a divergence serial
  structurally cannot match. The claim "cancelling a scope cancels its nested scopes" is therefore true
  **at checkpoints** (a running or later-parking grandchild), not for an already-parked one —
  `docs/reference/concurrency.md` now says so.
- A forever-blocking `defer` in a **non-cancelled** task is reported by BOTH engines but with different
  text/span (serial: `recv on an empty channel: deadlock …` at the recv site; M:N: the nursery-level
  deadlock message at line 1). Message-only here (both fault, both exit non-zero) — but see **N6g**: when
  the value WOULD have arrived from a live sibling the divergence is outcome-level, not cosmetic.
- `--serial` has no preemption and runs `spawn`s in order, so a CPU spinner spawned BEFORE its faulting
  sibling never yields and the sibling never runs (`timeout`), while M:N cancels it promptly. Spawn the
  faulter first and both engines cancel promptly (verified). Pre-existing cooperative-engine property, not
  a cancellation bug — the back-edge checkpoint is intact.
- `MnSched::take_runnable` checks `c.terminate` BEFORE it looks at `c.global` (and the 1-in-61
  `GLOBAL_CHECK_INTERVAL` fast path pops `global` without a terminate check). Inert known hazard: no repro
  exists, because `terminate` is latched only by `finish` when every scope is done (no fiber can then be
  owed an unwind) or by `flag_deadlock`, which drains `parked` itself and — after the N6g fix — can only
  fire for a cancelled scope with no undrained park **and no cancel-wakeable demoted fiber** (i.e. when
  nothing is owed an unwind that a cancel could still deliver). A demoted fiber unwinding after `terminate` runs on
  its own thread and never re-enters `take_runnable`. No failing test, so no change.

### N5 status after the N6 fix — UNTOUCHED then, and CLOSED as not-a-bug 2026-08-06 (see the N5 section)
A **genuine** deadlock (every fiber parked, nothing cancelled, nothing able to arrive) still tears the
parked fibers down without running their `defer`s — on **both** engines, and that is now the stated
contract: Go's deadlock `fatal error` skips its `defer`s too, and CPython does not even get that far
(it hangs). Serial reports it from
`run_scheduler_level`'s `None` arm, which **never** routes through the cancel drain; M:N's `flag_deadlock`
is unchanged. So the engines still agree and no new divergence was created. (A *nested* level's deadlock
arriving at the outer level as an ordinary child error DOES now cancel-and-drain the outer level's parked
siblings on serial — it already did on M:N. That is a deliberate convergence, not N5.)

## Audited residuals — pre-JIT hunt wave 5 (2026-07-13)

Everything below was **found, reproduced on both engines, and deliberately NOT fixed** in the wave-5
sweep (13 bugs fixed, main `0741a0b`). Each is either an accepted design consequence, a
documented-but-unusable surface, or a safe over-rejection. Recorded so they are decisions, not
surprises — **re-read this before the JIT freeze**, since a JIT bakes in whatever is true at freeze time.

### 0. Task stdin: serial-vs-M:N divergence + the false EOF — **BOTH FIXED (2026-07-14); stdin is now SHARED**
Two bugs, one seam. Stdin was **entry-task-owned**: every other task was handed `Stdin::Empty`, so
`read_line`/`input` inside a task returned `None` — a **false EOF**, while the entry task still had
unread lines queued. And that rule was enforced at exactly ONE task-entry seam (`swap_ctx` — the
`spawn:`/nursery fiber path), while the cooperative `Executor` drain runs a submitted closure **inline
on the entry Vm** (`src/vm/netio.rs`, no `swap_ctx`) — so on serial the task read *and consumed* the
entry's stdin while M:N's workers reported EOF: an **accidental serial≠M:N divergence**, the invariant
the whole parity oracle rests on.

> **Correction (2026-07-14 audit), still standing as of 2026-08-10:** this entry used to call it "the
> only known serial≠M:N divergence", and the 2026-07-14 correction answered that `std.net` was a
> **standing, deliberate** second one. **That is still true.** `W7-40` (2026-08-10) narrowed the
> divergence — a would-block socket op now blocks in place from top-level `main` on the default engine
> instead of erroring — but `--serial` deliberately keeps the `Err`, because its single thread is the
> one that would run the peer, so blocking it is a hang with no escape (measured: rc=124). See §Net and
> `W7-40`. An earlier draft of this correction claimed the divergence was gone; it was written against a
> first attempt that was reverted for exactly that reason.

The semantics is now **shared stdin** (Go's `os.Stdin` / Python's `sys.stdin`): ONE source, any task may
read it, a line goes to **exactly one** task (never duplicated, never dropped), WHICH task gets it is
**nondeterministic** on both engines, and `None` means genuinely exhausted. The `Empty`-for-tasks rule
was fake determinism protecting the oracle at the user's expense — the same mistake the interactive-CLI
milestone removed from stdout. The oracle bends; the language does not. `Stdin::Empty` survives only as
a legitimate host config (an embedder with no stdin). Killed at every task-entry seam — `swap_ctx`
(field deleted), `spawn_worker` (shares the handle), the netio inline drain (park reverted) — and pinned
by `parity_{spawned,executor}_tasks_share_stdin_exactly_once` (line-multiset, not exact stdout: the
assignment is nondeterministic by design) + the real-binary `task_reads_piped_stdin_{mn,serial}`.
**Lesson for the remaining hunt: an invariant enforced at one seam is not enforced — enumerate every
task-entry path.**

**New v1 limit it introduces:** `read_line`/`input` are deliberately not `Kind::Blocking` (the off-heap
`OffloadHost::read_line` is `unreachable!`), so a task blocked in a read once **pinned an M:N core worker** —
K blocked readers occupied K workers until stdin produced lines. Superseded 2026-09-20 (TICKET-151, W14-40):
they are `Kind::HostWait` and the engine demotes the worker around the read, so a blocked reader no longer pins one.

### 3. Three over-rejections introduced by the Go-model int→float fix
The wave-5 widening fix (untyped **constant** adapts; a typed int **value** never does) rejects three
constructs that are *not* unsound — it errs safe, but it errs:
- an aliased-collection annotation,
- a generic-erased method param,
- a fn-typed-field call.

All three **reject valid code rather than accept invalid code**, and have **zero in-repo users**. Upgrade
path recorded in the test doc-comments. Revisit only if a real program hits one.

### 4. A module bind shadows a same-named USER ctor — DIAGNOSED, alias is the cure (downgraded)
The wave-5 reserved-module-bind gate (`module name 'int' is reserved (builtin) — alias it: …`) covers
the **34 reserved/builtin** names. It does **not** cover a *user* `struct`/`enum` ctor: a module named
`Point` still wins over a user `struct Point` in expression position (same root cause as the fixed
`import std.str` bug — the bind lands in the VALUE namespace).

**But the blast radius is far smaller than first recorded, and this is now a closed decision.** Unlike a
reserved name — which the module bind *silently destroyed* — a shadowed user ctor is a **hard type error
at the call**, so no program can run wrong; and `import lib.Point as pt` is the cure, which is exactly
what Python does. That is normal shadowing with a diagnostic Python doesn't even give you. The only real
defect was the *message*: the bare `module Point is not callable` never said where your ctor went. Fixed
— the not-callable arm now names the collision (`module bind 'Point' shadows the same-named type
'Point' — alias the import: …`); test `module_bind_shadowing_user_type_names_the_collision`.

A separate **module namespace** (module names legal only in field position) remains the principled fix
and would remove the collision entirely, but it is a resolver change and buys only the loss of an alias
keystroke. Not planned.

### 5. Never-hunted surfaces (the two biggest remaining pre-JIT risks) — **BOTH SWEPT 2026-07-25 (wave 6); the SANITIZER half is still unbuilt**
Five hunt waves had swept the typed feature surface, the stdlib, concurrency, and the front-end, leaving
**two surfaces never audited at all** — the memory-fragile ones. **Wave 6 (2026-07-25) swept both at the
value level.** Status now:
- **GC + `unsafe`** — swept at the value level and came back **CLEAN**: ~250 targeted programs over the
  freshly-rewritten layout (`Fields` inline/spill, `tid`→name, mark bitset, boxed `Obj::Module`, 8B
  `Value`) + a 220-program randomized `--serial`-vs-M:N differential fuzz, 0 divergences / 0 crashes /
  0 wrong values, plus a source audit of the bitset, SSO `from_utf8_unchecked`, and `str_intern`. Two
  LATENT (currently unreachable) traps recorded in the wave-6 session log. **Still unbuilt and still the
  real residual risk: Miri / ASan / TSan** — Tier-1 lever #3 in [`bug-discovery.md`](../../contributing/bug-discovery.md). A
  value-level sweep cannot see UB or a data race; clean here means "no observable wrong value", not "no UB".
- **FFI** — swept, and it **did NOT come back clean**: 4 real defects (a `recover:`-proof VM panic on a
  zero-field struct at the boundary, a SIGSEGV on any *stored* callback, silent UTF-8 mangling in
  `load_str`, and two dead/absent extern-name collision guards), plus a void-returning callback being
  unspellable at all. See the 2026-07-25 session log (W6-5, W6-8, W6-14, W6-6, W6-11) — **all now
  fixed**, the stored-callback SIGSEGV last (W6-8, 2026-07-27: the trampoline is leaked + poisoned, so
  the still-deferred feature aborts with a named message instead of executing freed memory, on the
  calling thread and on any other). The libffi
  `Cif` heap-pin SIGSEGV precedent held: FFI UB is layout-dependent and invisible to the value-level
  oracles — W6-8's fix is likewise gated on a real-binary subprocess test, not a stdout golden.

Neither surface is reachable by the panic-fuzzer, the CPython differential, the DSA judge, or two-engine
parity — all four are *value*-level oracles. **Next before freezing: build the sanitizer lever** (it is the
only thing that can clear the GC/OS-thread `unsafe` surface, which wave 6 could only clear behaviorally).

## Dependency versions (as of 2026-07-07)
All four are **major (semver-incompatible)** bumps — cargo shows them but won't auto-take. `cargo audit`
(2026-07-07, 152 deps) = **0 vulnerabilities, 0 warnings** → no security driver; do NOT bump
speculatively during the perf milestone.
- **libffi** 3→5 — **do not** bump speculatively (FFI UB is layout-dependent; the Cif heap-pin caused a
  SIGSEGV before). Highest risk, ~zero payoff.
- **ureq** 2→3 — DONE 2026-09-20 (TICKET-153, W14-30b): `std.request` now runs on ureq 3.4.2.
- **socket2** 0.5→0.6, **libloading** 0.8→0.9 — skip until a needed feature forces it.
