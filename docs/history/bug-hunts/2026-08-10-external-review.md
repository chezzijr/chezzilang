# Gaps archive: external review batch 1 (W7-39..W7-55) (moved verbatim from `docs/history/bug-hunts/gaps-archive.md` lines 9921-12484)

## Session log — 2026-08-10 (external review, batch 1: 6 re-verified findings — 3 FIXED, 3 filed OPEN — plus 2 found while fixing)

Nine independent external testers wrote programs against Chezzi and reported what broke. **Six of the
findings were re-verified on HEAD `2a27697e`; all six reproduce.** Two are fixed on this branch
(`W7-39`, `W7-40` — the latter deliberately NARROWER than first attempted); four are filed open
(`W7-41`, `W7-42`, `W7-43`, `W7-44`). Two more were found while doing the work — `W7-45`, and the
smaller items in the last subsection. (Of those, `W7-41`, `W7-42`, `W7-43` and `W7-45` have since been
**fixed**; `W7-44` is **fully fixed 2026-08-14 — BOTH halves**, docs and portable entropy.)

**Two of the three fixes first written were WRONG, and the full suite was green for both.** `W7-41`'s
was reverted whole (it rejected every `Box[List]`/`Box[tuple]`/`Box[struct]` `==`); `W7-40`'s over-reached
and hung `--serial` net programs and starved the shared `Executor` pool, and was narrowed to top-level
`main` on the default engine. **Both were caught by adversarial review running the PRE-change binary on
neighbours no test covered — neither by 4164 green tests, nor by two-engine parity, nor by clippy.** The
common shape: a change that WIDENS what is rejected, or NARROWS when an error is returned, is not
tested by the suite that passed before it, because the suite contains no example of the thing it newly
affects. Each row below carries its own version of that lesson.

**The batch's own theme is this file's oldest one, arriving from outside.** All three fixed findings
are *a rule that held on some arms of a set and not the others*: one nursery seam that installed a
cancel token without the `scope_ancestors()` half every other seam pairs it with; four of `std.net`'s
five blocking ops left behind by a fallback `connect` already had; one of the five things a `where`
receiver bound gates (`<`, `+`, hashing, generic bounds, `==`) that never asked. Wave 6's meta-finding
— **"a fix applied to SOME arms of an N-way set"** — is still the highest-yield lever, and outside
testers hit three more instances of it without knowing it had a name.

### W7-39 — a nested `Executor`'s jobs never observed the outer `shutdown_now()` — **FIXED 2026-08-10**

**Repro.** An outer `Executor` submits `outer()`; `outer` creates `inner := Executor()`, does
`inner.submit(nap)` where `nap` is `time.sleep_ms(8000); print("nap finished")`, then itself sleeps
8000 ms. `main` sleeps 50 ms, calls `ex.shutdown_now()`, prints `done`.

**Measured, release binary, default (M:N) engine:**

| | output | wall |
|---|---|---|
| pre-fix | `done` then **`nap finished`** | **8.005 s** |
| post-fix | `done` only | **0.056 s** |
| flat control (no inner executor), both | `done` only | 0.055 s |
| `--serial`, both | `done` only | 0.054 s |

`--serial` was **already correct** — it queues and drops on `shutdown_now` — so this fix brings M:N
*into* engine agreement rather than out of it.

**The ancestors are SPLIT, so they did not decide it.**

| ancestor | program | measured |
|---|---|---|
| CPython 3.14.6 | nested `ThreadPoolExecutor`, outer `shutdown(cancel_futures=True)` at 50 ms, `wait=False` | the **call** returns at 0.051 s, but the **process** takes **8.037 s** and prints both job lines |
| CPython 3.14.6 | same, `wait=True` | 8.001 s |
| Go 1.26.5 | `context.WithCancel(parent)` parent→child, parent cancelled at 50 ms | `outer cancelled at 50ms`, `inner cancelled at 50ms`, main returns at 251 ms |

**CPython does not cancel** — it only declines to block the caller; the non-daemon pool threads are
joined at interpreter exit. (An external report's "63 ms" measured the *call*, not the process. Measure
the process.) **Go propagates.**

**What decided it is Chezzi-vs-Chezzi.** Pre-fix, the outer job's OWN `sleep_ms(8000)` *was* cancelled
at 50 ms — `outer finished` never printed — while the *identical* sleep one executor deeper ran the
full 8 s. One spelling of one wait had two cancellation rules depending on nesting depth. That is
`W7-16`'s ruling ("an executor that disagrees with the nursery beside it is the defect") applied one
level down, and `W7-16` already resolved against CPython on exactly this axis. Go concurs.

**Cause.** `Vm::prepare_eager_job` (`src/vm/sched.rs:4050`) installed `worker.cancel = Some(core.cancel)`
and left `worker.cancel_outer` empty — **the one seam in the tree that installed a cancel token without
the `scope_ancestors()` half**, which `spawn_shell` (`sched.rs:875`, `:881`) and `run_one_fiber`
(`sched.rs:1575-1585`) both pair it with. So an inner executor's jobs carried the chain `[inner.cancel]`
and could never observe the outer trip; `drain_live_executors` (`src/vm/netio.rs:3579`) then joined the
un-shut inner core at program exit, which is where the 8 s came from.

**Fix.** The chain is captured at the executor's **construction** (`Op::NewExecutor`,
`src/vm/exec.rs:2382`) into `ExecutorCore::creator_cancel` (`src/vm/core.rs:775`) and installed from
the CORE at dispatch (`src/vm/sched.rs:4091`) —

```rust
// Op::NewExecutor
creator_cancel: match self.eager_core.is_some() {
    true => self.scope_ancestors(),
    false => Vec::new(),
},
// prepare_eager_job
rw.worker.cancel_outer = core.creator_cancel.clone();
```

**Keyed on the CREATOR, never on the submitter** — the first cut read `self.scope_ancestors()` at
`submit`, and because an `Executor` crosses the airlock by `Arc` any job can `submit` to any executor
it can name, so an unrelated executor's `shutdown_now()` killed a job belonging to `main`'s executor
and `main`'s own graceful `shutdown()` returned with the work silently dropped. Pinned by
`a_job_submitted_to_mains_executor_survives_another_executors_shutdown_now_mn`.

Gated on `eager_core` so decision **A2**'s "an `Executor` is **detached**" survives intact: detached
from NURSERIES, not from an enclosing executor job. An executor created by `main` or by a
`parallel:`/`spawn` fiber gets an EMPTY chain and behaves exactly as before.
`Vm::scope_ancestors` (`src/vm/exec.rs:1584`) already severs inside a `defer`, so `defer ex.shutdown()`
in a cancelled job still submits *uncancelled* cleanup. Docs moved in the same change:
`docs/reference/concurrency.md`'s `shutdown_now()` table row, a measured Python/Go block, a "what detached means,
precisely" block under A2, and `docs/reference/stdlib.md`.

**Follow-up, same branch: the inherited chain is STICKY, and that made a later `submit` VANISH.**
`creator_cancel` is set once and never reset — deliberate, and Go-consistent (a derived
`context.WithCancel(parent)` stays cancelled once the parent is). The consequence is that after the
creating job's executor is `shutdown_now`-ed, *every* job the inner core later dispatches starts
already-cancelled and dies at its first checkpoint. It did so **silently**: the handle is a value, so
`main` may hold the only reference, and `main`'s own graceful `shutdown()` — which promises to wait for
its work — returned having run nothing:

```chezzi
fn work():
    time.sleep_ms(10)          # a cancellation checkpoint — REQUIRED to observe this
    print("inner job ran")
fn jobber(ch: Channel[Executor]):
    inner := Executor()
    ch.send(inner)
    time.sleep_ms(2000)
ch := Channel[Executor](1)
outer := Executor()
outer.submit(fn(): jobber(ch))
inner := ch.recv()
time.sleep_ms(50)
outer.shutdown_now()
inner.submit(work)
inner.shutdown()
print("main done")
```

→ pre-fix `main done`, **rc=0**, `inner job ran` never printed; with `outer.shutdown()` (graceful) the
same program printed `inner job ran` / `main done`. **Keep the stickiness, drop the silence:** a
`submit` whose inherited chain is already tripped now FAULTS, reusing the existing post-shutdown reject
(`src/vm/netio.rs:3382-3387`, the advisory pre-wiring check in `executor_method`) —
`submit on an Executor whose creating job was cancelled (it no longer accepts work)`. The core's OWN
`cancel` is deliberately not checked there: `shutdown_now` sets `shut` first, so the neighbouring
message already owns that case and adding it would double-report. `--serial` never sets `eager_core`,
so `creator_cancel` is always empty there — no parity delta. Pinned by
`submit_to_an_executor_whose_creating_job_was_cancelled_faults_mn` plus two negative controls (graceful
outer shutdown; an executor created by `main` after an unrelated `shutdown_now`).

**The checkpoint trap — a test written without one is a FALSE GREEN.** With `work` a bare `print` and
no `sleep_ms`, the job finishes before reaching any cancellation point (D4 is cooperative, not
pre-emptive) and the bug does not show at all. This trap already burned this branch once.

**Lesson — a one-line install is only safe once you have checked every path that WRITES the same
field.** `run_one_fiber` (`sched.rs:1575-1585`) unconditionally overwrites **both** `cancel` and
`cancel_outer` on every fiber swap-in. It does not clobber this install only because an eager job runs
through `ReadyWorker::run_outcome` → `invoke` directly on the pool thread and never enters the fiber
scheduler. Had eager jobs been fibers, the fix would have been silently overwritten and the test would
have stayed red with no indication why.

### W7-40 — `std.net`'s blocking ops worked only on an M:N worker fiber, so the hello-world TCP server could not be written — **FIXED 2026-08-10 (narrowed, after a first attempt was measured to cause two hangs)**

**Repro.** `net.listen("127.0.0.1:PORT")` then `l.accept()` from top-level `main`, **default engine**:

```
listening
Err('accept would block: std.net sockets require the --parallel engine')
```

…while running the parallel engine. Same inside an eager `Executor` job. Sockets worked only inside a
`spawn`, which was documented nowhere — and the `spawn` workaround self-deadlocks in the obvious shape
(see the last subsection).

**Ancestor — Go, measured.** `net.Listen` + `ln.Accept()` **on the main goroutine**, dialer in a
`go func()` after 50 ms:

```
[   0.1ms] listening on 127.0.0.1:38541 (main goroutine)
[   0.1ms] calling ln.Accept() on the main goroutine...
[  50.5ms] dialed
[  50.5ms] Accept() returned: 127.0.0.1:58036
[  50.5ms] Read() -> "hi" err=<nil>
```

Accept blocks and succeeds. Chezzi errored.

**Cause — a field whose name reads as a global mode but is not one.** `Vm::mn` is assigned in **exactly
one place**, `Vm::spawn_shell` (`src/vm/sched.rs:877`; `Vm::new` sets `mn: None`, `src/vm/exec.rs:169`).
So `mn.is_some()` means *"this Vm is an M:N worker SHELL"* — **not** *"the parallel engine is on"*, which
is the separate `Vm::parallel` (default `true`). `main` runs `vm.run()` on the host thread and eager
`Executor` jobs come from `Vm::new`; neither has `mn`. `park_on_fd` parks only under
`mn.is_some() && native_reentry == 0`, so both fell straight through to the error return.

**`connect` already handled this** — it has had an explicit `mn == None` arm calling
`block_until_connected` (`src/vm/netio.rs:400-416`, `:501`) — and only `read`/`read_bytes`/`write`/
`accept` were left behind. The `W7-22` meta-shape again: a fix applied to some arms of an N-way set.
(`connect`'s arm is *broader* than what shipped for the other four — it blocks on every engine — which
is its own smaller item, recorded in the smaller-items subsection below.)

**The first attempt over-reached, and was measured to cause two regressions.** It deleted the four
gates outright and let every non-worker context block in place, on the argument that a caller with no
`mn` "owns its thread". **That argument is false for two of the contexts it newly covered**, and its own
doc comment asserted it.

**R1 (critical — a clean `Err` became a permanent hang).** `--serial` has exactly ONE thread, and
blocking it starves the very fiber that would make the fd ready. Measured on the over-reaching binary:

| program | pre-change | over-reaching version |
|---|---|---|
| `timeout 15 chezzi run --serial examples/echo_server.chz` | rc=0 | **rc=124** |
| `timeout 15 chezzi run --serial examples/socket_timeout.chz` | rc=0 | prints `accept: timeout`, then **rc=124** |

**No escape exists**: `--timeout` is rejected alongside `--serial` (`src/main.rs:660`), and no other
fiber can run to deliver a cancel.

**R2 (high — same shape, on the shared pool).** An eager `Executor` job does **not** own its thread: it
runs on the bounded, process-wide pool (`pool::submit`, `src/vm/pool.rs:77`; sized by
`vm::worker_count()`, `src/vm/mod.rs:304`, which never grows), and the `mn.is_none()` path skipped
`demote_socket_enter`, so no replacement worker was spun. Measured at `CHEZZI_THREADS=1`: job A
`accept`s, job B (submitted after) `connect`s → **rc=124**; at ≥2 threads it passed. It also starves
`parallel:` nurseries sharing that pool.

**What actually shipped — one predicate, `Vm::may_block_socket_in_place` (`src/vm/netio.rs:2044`):**

```rust
(self.mn.is_some() && self.native_reentry > 0)                   // the original in-callback demote
    || (self.parallel && self.eager_core.is_none() && self.is_counted_party())
```

`is_counted_party()` (`netio.rs:1985`) is `owns_os_thread() && native_reentry == 0`, and
`owns_os_thread()` (`:2000`) is `mn.is_none() && mn_enlist_sched.is_none() && scheduler_stack.is_empty()`
— so the second disjunct is **top-level `main` on the default engine, and nothing else**. The four ops
(`netio.rs:617`, `:817`, `:959`, `:1089`) restore the **verbatim** pre-existing
`Err("<op> would block: std.net sockets require the --parallel engine")` for every other context.
`Vm::demote_block_socket` keeps its `Option<Arc<MnSched>>` (`src/vm/sched.rs:1352-1358`, `:1379-1383`,
`:1414-1416`) — that is exactly what lets `main` use the loop, whose wait is engine-neutral anyway
(`wait_fd_ready` is `libc::poll(2)` on the raw fd, not the netpoller). `connect` is untouched.

**So the shipped scope is: `accept`/`read`/`read_bytes`/`write` from top-level `main` on the default
engine now BLOCK and succeed (Go-identical). `--serial` and eager `Executor` jobs keep the immediate
`Err`.** The `std.net` serial≠M:N divergence this file has named since 2026-07-14 is **narrowed, not
closed** — R1 is the measured reason it must stay: on a single-threaded engine there is no peer to make
the fd ready. (§Net and §"Audited residuals" wave-5 item 0 both say so; an earlier draft of this row and
of both of those claimed the divergence was gone, written against the reverted attempt.)

Two consequences that DO hold, within that scope: a never-ready fd on `main` is now a **HANG** rather
than an `Err` (Go-identical; the escapes are the op's own `timeout_ms` and cancel), and a socket-blocked
`main` is deliberately **not** registered as a quiescence party — the `time.sleep_ms` precedent, since a
registered wait on *external* readiness is a false-deadlock generator, so it can only ever delay someone
else's deadlock verdict, never fabricate one (`parked-is-not-stuck`).

**Verified post-narrowing, release binary.** `--serial`: `echo_server` rc=0, `socket_timeout` rc=0 (now
printing the two engine `Err` lines), `echo_server_spawn` rc=0. Default engine: all three unchanged.
`CHEZZI_THREADS=1` executor shape: rc=0 in **6 ms** with a prompt `Err`. Accept-on-`main` with an
external `python3` peer: `accepted` / `Ok('hi')` on the default engine, the `Err` on `--serial`.
`cargo test --lib` **3952 passed**, clippy clean. The in-callback demote fences (`d5_owe3_path_c_*`,
`net_read_timeout_bounds_the_in_callback_demote_path`) and the three `W7-18`
`timeout_aborts_a_netpoller_*` fences stay green. **The narrowed behavior is now pinned by tests that
assert the `Err` verdict on `--serial` and inside an `Executor` job**, so the next person who widens
this has to face them.

**Lesson — a change that NARROWS when an error is returned is not tested by the suite that passed
before it.** The full suite was **green with both regressions present**, because nothing tested sockets
on `--serial` and nothing tested a socket op inside an `Executor` job under a constrained pool: the
suite has no example of the context the change newly affects, which is definitionally true of every
error-narrowing change. Both regressions were caught by **adversarial review running the PRE-change
binary** — not by 4164 tests, not by two-engine parity, not by clippy. This is `W7-41`'s twin: same
day, same branch, same failure mode (a green gate over an unexercised context), and in both cases the
only detector that worked was measuring the old binary against the new one on a program neither the
suite nor the report contained.

**Second lesson, from the brief — a "this branch is dead" argument that is off by one quantifier
deletes the right code and teaches the wrong invariant.** The work was briefed as *"the four gates are a
tautology — reaching them implies `mn.is_none() || native_reentry > 0`, so their `else` is dead"*.
**False as stated.** `mn.is_some() && native_reentry > 0` is not implied by
`mn.is_none() || native_reentry > 0`; the `else` is reached on exactly `mn.is_none()`, which is not dead
at all — it is both the bug's surface AND, for `--serial` and `Executor` jobs, the surface that had to
be preserved. Acting on the brief as written is what produced R1 and R2. Third, smaller:
`socket_read`'s call sits inside a `loop`, so it must stay a `return` where the other three are tail
expressions — converting all four uniformly is an `E0308` build error, which is the good outcome.

### W7-41 — a `where` receiver bound is enforced for `<`, `+`, hashing and generic bounds but NOT for `==` — **FIXED 2026-08-11** (a first attempt 2026-08-10 was reverted the same day; the ORDER is the lesson)

> **THE LESSON, before anything else: widen `Eq` FIRST, guard second.** A fix for this row was written
> on 2026-08-10, passed the entire suite (`cargo test --lib` 3952, `chezzi test tests/chz/` 424 on both
> engines, clippy clean) with the repro correctly rejected — and **broke working programs en masse.**
> Adversarial review caught it by running the *pre-change* binary on neighbours no test covered. The
> guard was *"type declares `eq` ⇒ require `satisfies(ty, Eq)`"*; `Eq` was granted intrinsically to only
> the four scalars, so `where T: Eq` was **unsatisfiable for every type whose `==` is structural**, and
> with `fn eq(self, o: Box[T]) -> bool where T: Eq` these all printed `true` before and became
> `cannot compare … for equality`: `Box([1,2])`, `Box((1,2))`, `Box(b"ab")`, `Box(Some(1))`,
> `Box(P(1))` for any plain struct `P`.
>
> **Widening `Eq` was never a prerequisite bolted on beside the fix — it is the fix's other half.**
> `satisfies(ty, "Eq")` and this row's guard are the same question from two directions, and the reverted
> attempt asked it while `Eq` still meant "one of four scalars", so it could only be wrong. The 2026-08-11
> work therefore lands in a **load-bearing order**: Stage 1 makes `Eq` satisfaction mean what `==`
> accepts, and only then does Stage 2 insert the guard. An intermediate commit in the other order
> re-ships the reverted bug verbatim.
>
> Three further traps, all real, all still worth reading before touching this area:
> 1. The guard must not fire on the in-tree **ordinary-method escape hatch** (`enum Opt[T]: fn eq(self, x: T)`,
>    `struct H[T]: fn eq[U](self, o: U)`) — those are not `(self, Self) -> bool`, so a full `satisfies`
>    call fails on legal code. **What shipped extracts only the `where_bounds` walk**, never
>    `satisfies_methods`' signature-shape check, for exactly this reason; both hatches were verified
>    byte-identical to the pre-change binary.
> 2. A **one-sided** guard is an asymmetry hole: `n == m` plus co-inhabitable args ≠ identical args (the
>    `int`/`float` and `bytes`/`bytearray` cross arms recurse in), so it must gate both operands. It does.
> 3. **The obvious fixture does not exhibit the bug.** An `eq` body of `self.val == o.val` falls back to
>    structural equality and exits 0. The fault needs a body that calls the *bounded* operation
>    (`self.compare(o)` → `<`, or `.hash()`). A test written from the fixture alone is a false green —
>    one was, and the 2026-08-11 review re-ran that false-green fixture on the pre-change binary to
>    confirm the new one is genuine rather than trusting the claim.
>
> **Meta-lesson (the repo's own `rule-fires-is-not-rule-is-right`): a new checker REJECT must be proven
> against its own premise — run the PRE-change binary on the `ok()` neighbours the premise implies, not
> only on the case being fixed.** A green suite proves nothing about a widening; this suite contained no
> `where T: Eq` over a non-scalar, so it could not. That discipline is what found **five Criticals and
> four Importants across four review rounds** in the 2026-08-11 work while `cargo test` sat at 4233/0 —
> see *"How it was actually fixed"* at the end of this section.

**Repro** — check-clean, then a runtime fault on the pre-change binary: the
`checker-superset-of-compiler` soundness class, which two-engine parity is structurally blind to.

```chezzi
struct Tag:
    n: int

struct Box[T]:
    val: T
    fn compare(self, other: Self) -> int where T: Comparable:
        if self.val < other.val:
            return -1
        return 0
    fn eq(self, other: Self) -> bool where T: Comparable:
        return self.compare(other) == 0

a := Box(Tag(1))
b := Box(Tag(2))
print(a == b)
```

**Pre-change binary (`3bc31bfb`), verbatim** — (the earlier printing of this row spelled the two
bindings on one line with a `;`, which Chezzi does not lex, so its quoted line numbers were never a
measurement of the program above; these are):

```
chezzi check → ok: no type errors
chezzi run   → runtime error (line 7, col 12): struct 'Tag' has no 'compare' method
                 at compare (called at line 11, col 16)
                 at eq (called at line 15, col 7)
```

**Post-change binary (`ef3995cb`), verbatim:**

```
chezzi check → type error (line 15, col 7): cannot compare Box[Tag] and Box[Tag] for equality — Box[Tag]'s `eq` requires Tag: Comparable
               chezzi: 1 type error
```

…and the satisfying instantiation still works: the same `Box` over `int` runs and prints `false` /
`true` for `Box(1) == Box(2)` / `Box(1) == Box(1)`.

It leaked identically through `!=`, `[a] == [b]`, `Some(a) == Some(b)`, and (separately) `in`.

**Ancestor — Rust owns conditional conformance. `rustc 1.97.0`, verbatim on the mirror
`impl<T: Ord> PartialEq for Boxy<T>`:**

```
error[E0369]: binary operation `==` cannot be applied to type `Boxy<Tag>`
note: an implementation of `Ord` might be missing for `Tag`
help: consider annotating `Tag` with `#[derive(Eq, Ord, PartialEq, PartialOrd)]`
```

Positive control (`Boxy(1) == Boxy(2)`) compiles and prints `false`. **Compile-time rejection, and a
satisfying instantiation still works** — which is the shape the fix has to reproduce.

**Cause — a deliberate deletion that went stale under a later feature.** `<` and `+` route through
`ordering_allowed`/`op_overload_result` → `satisfies` → `satisfies_methods`, whose `where_bounds` walk
fires. `==` routes through `may_be_equal` (`src/checker/proto.rs:813`), whose nominal arm compared
**name + arity + arg co-inhabitance only** and never called `satisfies`. `proto.rs` and `pattern.rs`
both carried the argument that *"a user `eq` overload adds NOTHING to ask here: it only ever applies to
a same-type pair, which `may_be_equal` already accepts"*, and this file called the deleted
`equality_allowed` disjunct **"provably dead"**. **Conditional conformance — a `where` clause on the
method, landed after M23 — invalidates exactly that premise:** `Box[Tag] == Box[Tag]` IS a same-type
pair whose `eq` does not cover it. (The prose at §"what the checker owns for `==`" is corrected in the
same commit.)

**The attempted fix, and the measurement that killed it.** Checker-only (the runtime dispatches off the
operands' heap tags and is type-blind by design): a `Checker::eq_overload_applies` called on both
operands from `may_be_equal`'s nominal arm, plus `|| !self.may_be_equal(…)` on the `In` arm's list/set
element and map key checks. One guard, closing `==`, `!=` and every container/payload nesting, because
`List`/`Set`/`Option`/`Map`/`Result`/`Tuple` all recurse into that single arm. It **rejected the repro
correctly** and passed everything: `check` reported 3 errors (direct, `!=`, `List` nesting),
`chezzi test tests/chz/` **424 passed on both engines**, `cargo test --lib` **3952 passed**, clippy clean.

**And it broke working programs.** The guard's core was *"the type declares an `eq` ⇒ require
`satisfies(ty, "Eq")`"*. `Eq` is granted intrinsically to **only the four scalars**
(`src/checker/proto.rs:1541`), so `where T: Eq` is unsatisfiable for every type whose `==` is
structural. With `fn eq(self, o: Box[T]) -> bool where T: Eq`, measured against the pre-change binary:

| program | pre-change | with the guard |
|---|---|---|
| `Box([1,2]) == Box([1,2])` | `true` | `cannot compare Box[List[int]] and Box[List[int]] for equality` |
| `Box((1,2)) == Box((1,2))` | `true` | rejected |
| `Box(P(1)) == Box(P(1))`, plain struct `P` | `true` | rejected |
| `Box(b"ab") == Box(b"ab")`, `Box(Some(1)) == …` | `true` | rejected |

**Reverted.** The prerequisite is not a guard on the equality path — it is making `Eq` satisfaction
agree with what `==` actually accepts, which touches every `satisfies(_, "Eq")` caller. That is Stage 1
of the 2026-08-11 fix below, and landing it first is what made the second attempt hold.

**Three traps for the next attempt, all measured.**

1. **The obvious guard over-fires** beyond the `Eq`-grant problem: it also rejects the ordinary-method
   escape hatch that already exists in-tree (`enum Opt[T]: fn eq(self, x: T)`,
   `struct H[T]: fn eq[U](self, o: U)`). Those signatures are not `(self, Self) -> bool`, so `satisfies`
   fails and `Opt[int] == Opt[int]` is rejected. Needs the `Ty::Param`/`Unknown` operand test mirroring
   `validate_eq_shape`.
2. **A one-sided guard is an asymmetry hole.** `n == m` plus co-inhabitable args is NOT identical args —
   the `int`/`float` and `bytes`/`bytearray` cross arms recurse in — so a left-operand-only guard gives
   `Box(1) == Box(1.0)` and `Box(1.0) == Box(1)` different verdicts off one hook. Gate both operands.
3. **The obvious fixture does not exhibit the bug.** An `eq` body of `self.val == o.val` falls back to
   structural equality on the payload and exits 0; the runtime fault needs a body that calls the
   *bounded* operation (`self.compare(o)` → `<`, or `.hash()`). A test written from the fixture alone is
   a false green — the reverted change shipped exactly that test, with a docstring quoting a fault its
   own fixture never produced.

**The meta-lesson, and it is this repo's `rule-fires-is-not-rule-is-right`: a new checker REJECT must be
proven against its OWN PREMISE — run the pre-change binary on the `ok()` neighbours the premise implies,
not only on the case being fixed.** A green suite proves nothing about a widening: this suite contained
no `where T: Eq` over a non-scalar, so it was structurally incapable of catching the regression, exactly
as two-engine parity is structurally incapable of catching the underlying soundness bug. The precedent
is the **protocol-embeds** row above — a widening whose negative control was skipped shipped fully green
with a soundness hole. This one skipped the *pre-change* half of the control and shipped the mirror
image: a rejection with no soundness hole and no legal program left standing.

---

#### How it was actually fixed (2026-08-11, branch `w7-41-45-eq-bounds`, `09622b25..ef3995cb`)

**Stage 1 — `Eq` satisfaction becomes exactly what `==` accepts.** One rule, no membership table to
argue about:

```
satisfies(ty, "Eq")  ⟺  the structural-equality walk reaches no declared `eq` whose `where` bounds
                        fail under the receiver substitution
```

so containers, structs, enums, newtypes, tuples, `Option`/`Result` and `bytes` now satisfy `Eq` —
every one of which already had a working `==`. Chezzi's structural `==` was an automatic derive that
had never told the protocol system (Go gives structs `==` automatically; Rust `#[derive(PartialEq, Eq)]`;
Python `@dataclass(eq=True)`). `Ty::Param` is excluded **explicitly**: `may_be_equal(Param, Param)` is
`true` via the erasure arm, so a naive reading of the rule would make every unbounded `T` satisfy `Eq`.
The predicate is `Checker::eq_bounds_unsatisfied` (`src/checker/proto.rs:2631`) and it **FAILS CLOSED** —
its `None` feeds a `grant_intrinsic`, i.e. a promise the runtime can dispatch `eq`, so every
cannot-finish-the-proof path returns `Some` (refuse). The grant is pinned by the
`intrinsic_grants_all_have_vm_arms` ratchet (`src/vm/tests.rs:16337`), which **runs `x.eq(y)` on every
newly-granted kind on both engines** — the repo's own built-in proof this is not itself a
`checker-superset-of-compiler` bug. One VM change was needed for that to hold: the `Obj::Enum` dispatch
arm ended in a bare *has no method* with no `intrinsic_proto_method` fallback, and `Option`/`Result`
**are** `Obj::Enum` at runtime.

**Stage 2 — the guard**, one insertion at `src/checker/pattern.rs:2485`, before `may_be_equal`, gating
**both** operands and decorating the existing message. Because the predicate recurses into
element/payload/field types itself, that single site closes `[a]==[b]`, `Some(a)==Some(b)`,
`(a,)==(b,)`, `{"k":a}=={"k":b}`, newtype underlyings, enum payloads and struct-field nesting at once.
It is deliberately NOT inside `may_be_equal`, which is `&self`, non-emitting by contract, and recursive.

**Stage 3 — `W7-45`** (see that section).

**Deliberately dropped in the same work: the M23 rule "a type defining `compare` must define `eq` too".**
Its premise was falsified by measurement in **both** directions, both programs legal at rc=0:

```chezzi
# rule would FIRE, premise does NOT hold — a field-complete compare that agrees with structural == exactly
struct Pt:
    x: int
    y: int
    fn compare(self, o: Self) -> int:
        if self.x != o.x:
            return self.x - o.x
        return self.y - o.y        # no `eq` -> would be rejected. FALSE REJECT.

# premise HOLDS, rule would NOT fire — and this is the repo's own motivating example
# (tests/chz/spec/eq_protocol_test.chz:1-11)
struct Ver:
    maj: int
    pre: str
    fn compare(self, o: Self) -> int: return self.maj - o.maj
    fn eq(self, o: Self) -> bool: return self.maj == o.maj and self.pre == o.pre
```

So it never enforced coherence — it enforced *"you typed the word `eq`"*. Rust, the owning ancestor,
permits manual `Ord` + derived `Eq` (a clippy lint, not an error), and `docs/reference/syntax.md` already conceded
that the checker cannot verify `eq(a,b) == (compare(a,b)==0)`. Dropping it also removes a dead end:
declaring `eq` on a newtype is an unconditional error (`src/checker/setup.rs`), so for a
`compare`-declaring newtype the rule was **unfixable**. Measured, `struct Ver` with `compare` and no
`eq`: pre-change binary → two errors, *"type Ver does not satisfy Eq (missing method 'eq')"*;
post-change → accepted. **Ordering was NOT widened** — there is still no default structural `compare`
(Go has no struct `<`, Rust needs `#[derive(Ord)]`, Python `@dataclass` defaults `order=False`).

**What the green suite could not see.** Four adversarial-review rounds found **five Criticals and four
Importants** while the full suite stayed green throughout (4233/0 at the Stage-1 gate commit `92b9ce59`;
4236/0 at `ef3995cb`, 4024 lib + 212 integration over 17 targets, with `chezzi test tests/chz/` 466/466
identical on M:N and `--serial`). Every one was found by running the *pre-change* binary
on `ok()` neighbours derived from the premise, and re-verified by repro on the real binaries on both
engines. In order:

| # | finding |
|---|---|
| C1 | the walk used DECLARED, unsubstituted field/payload types — three faces: generic nominals not granted at all; **name capture**, a fresh check-OK-then-fault of the very class this row closes, introduced by the fix; and a context-dependent verdict (`Wrap[int]` satisfying `Eq` inside `fn outer[Z: Eq]` but not at module scope) |
| C2 | field lookup used `self.structs.get(name)?` rather than the miss-only `struct_shape`, and the enum arm had no owning-module fallback — so `import make from src.lib` **skipped the field walk entirely** and check-cleaned, while `import src.lib as L` on the identical program correctly rejected |
| C3 | the re-entrancy cap declined in the GRANTING direction: depth ≤15 correct reject, ≥16 check-OK-then-fault |
| C4 | the inner cycle guard was keyed on a BARE NAME, so a *different instantiation* of a name already on the walk was assumed sound and its fields never walked — **masked** at the first commit by an accidental over-reject that fixing C1 removed |
| C5 | the outer in-progress guard keyed on `Ty::to_string()`, so two structs with the SAME NAME in different modules collided and the coinductive assumption was applied to a type never in progress |
| I6–I9 | a depth backstop that refused SOUND programs and whose message was unreachable; a fail-closed over-refusal of a nested cursor; and a **new hang class** — the walk was path-based with no memo, so a struct with two fields of the same nested type was re-walked 2^N times. The fix wave's own timings on the intermediate release binary: N=20 0.59 s, N=22 2.4 s, N=24 9.5 s, N=26 37.6 s, against 0.003 s on the pre-change binary. Fixed by a per-query memo; **re-measured on the shipped binary here**, a 26-level two-shared-fields chain driven through a `[U: Eq]` bound is `ok: no type errors` in **0.003 s** (the same program is *type T26 does not satisfy Eq* on the pre-change binary, so it does exercise the new grant) |

C4 and C5 were **one** structural cause — two cycle guards that disagreed about what a type *is* — and
were fixed by merging them into a single identity-keyed mechanism rather than by patching each.

Two judgement calls were upheld under an independent attempt to disprove them, and both are worth
keeping: a cycle HIT answers `None` because it is a **coinductive assumption**, not a decline (rustc
1.97.0 compiles the mirror, measured); and **`Ty::Protocol` cannot be refused** — see `W7-52`, now
resolved as ancestor-correct (Go's own interface `==` does the same thing), not a bug.

**Residuals:** `W7-52` (`Ty::Protocol`, both halves) — **since RESOLVED 2026-08-12 (verdict) + FIXED 2026-08-12 (the `[T: Eq]` bound gap)** —
`W7-53` (the erased call site), **since FIXED 2026-08-12** (see the row), and `W7-54` (a function value
did not satisfy `Eq`, though `f == g` worked — Rust compiles the mirror), **since FIXED 2026-08-12**.

### W7-42 — a top-level `:=` re-declaration retypes a LIVE binding, so the checker validates a different program than the one that runs — **FIXED (2026-08-10)**

**Repro** — check-clean, then a `-> int` function returns a `str`:

```chezzi
x := 1
f := fn() -> int: x
x := "9"
print(f())      # 9   ← a str, out of a fn declared `-> int`
y: int = f()    # accepted
print(y + 1)    # runtime error: cannot apply Add to str and int
```

Inside a `fn` body the same program correctly prints `1`. **Module top level only — i.e. exactly where
scripts live.**

**Cause, both halves.** The checker models re-declaration as *retype one binding*: `Checker::declare`
(`src/checker/setup.rs:1747`) is an unconditional `insert` with no "already present?" branch. The
compiler models it **two different ways**: `collect_globals`'s `add` closure
(`src/compiler/mod.rs:940-945`) is idempotent BY NAME, so the second `x` reuses the first `x`'s slot and
a closure's late-bound `GetGlobalSlot` reads the new value; while `FnComp::add_local`
(`src/compiler/mod.rs:6512`) always pushes a FRESH slot, so a fn-local re-declare is a new cell. **The
checker agrees only with the local backend.** `setup.rs:1912-1917` already names this exact class in
`is_local_binding`'s doc comment — *"the checker would validate a different program than the one that
runs"* — for a neighbouring case.

**The ancestors disagree with each other, so this needed a DECISION, not a lookup.** All three were
**re-run at implementation time**, same shape as the repro; verbatim outputs:

| ancestor | program | measured output |
|---|---|---|
| CPython 3.14.6 | `x = 1` / `def f() -> int: return x` / `x = "9"` / `print(f())` | `9`, rc=0 — late binding. The *value* matches Chezzi's scripting ancestor; Python is dynamic, so its `-> int` is an unenforced hint and there is no static claim to violate |
| Go 1.26.5 | `x := 1` / `f := func() int { return x }` / `x := "9"` | **compile error**, rc=1: `./a.go:8:4: no new variables on left side of :=` + `cannot use "9" (untyped string constant) as int value in assignment` |
| rustc 1.97.0 | `let x = 1;` / `let f = move \|\| -> i32 { x };` / `let x = "9";` / `println!("{} {}", f(), x)` | `1 9`, rc=0 — shadowing makes a FRESH binding, so the closure keeps the old one and no declared type is ever violated |

Neither statically-typed ancestor permits the check-clean lie: **Go rejects it, Rust makes the
re-declaration a fresh binding so the type never changes underneath a closure.** Chezzi did neither —
it took Python's late binding while keeping Go/Rust-style annotations, which is the bug.

**Decision (shipped): reject a module-scope re-declaration that CHANGES THE TYPE.** Python's late
binding is preserved for same-type rebinds (`x := 1` / `x := 2` still prints `2` through a
pre-existing closure), and only the unsound shape is rejected. The rejected alternative — a fresh
global slot per re-declaration, matching Rust and the fn-local path — is semantically cleaner but
**changes currently-working programs** (`x := 1; f := fn() -> int: x; x := 2; f()` would go `2` → `1`)
and touches the global-slotting perf seam.

**What shipped.** One checker-only carve-out beside the `const` one (`src/checker/sig.rs:2007`, an
`else if` on the same binding site so a live `const` still reports the const message and only that).
It fires when **all** of:

- `self.scopes.len() == 1` — the module-scope discriminator, which maps 1:1 onto the compiler's
  `is_global_scope()` (`src/compiler/mod.rs:6486`). **Not `in_fn_body`**: a top-level `if:`/`for:` body
  routes to `add_local`, and `in_fn_body` would over-reject it (measured legal, before and after);
- the previous and newly declared types **differ**;
- `declared` is **not a refinement of** `prev` — `merge_unknown(prev, declared) != declared`, using the
  existing refine-on-first-use structural merge (`src/checker/mod.rs:2524`), plus a bare
  `declared != Ty::Unknown` guard because `merge_unknown` early-returns `a` on an unknown *shape*.
  `x := []` then `x := [1]` is a script-writer refinement, not a retype, and stays legal; two
  *concrete* element types (`xs := [1]` / `xs := ["a"]`) are the same one-slot lie as any other and do
  reject. **This conjunct shipped WRONG first and was repaired the same day** (adversarial review):
  the original was a *two-sided* veto (`!contains_unknown_in_slot` on both sides), so an `Unknown`
  anywhere on *either* side silenced the rule and the headline defect stayed open in four shapes —
  `x := 1` / `f := fn() -> int: x` / `x := None` was check-clean and printed **`None`** out of a
  `-> int` fn (rc=0), as were `[] -> 42`, `None -> 42` and `1 -> []`. The relation wanted was always
  one-sided: *is `declared` a refinement of `prev`?*;
- `!self.inferring_ret` — return inference truncates errors and re-walks a body inside one open scope,
  so without the gate the rule double-fires;
- when `prev` is **the import's own binding**, the `import` is **source-EARLIER** than this `let`:

  ```rust
  (!(self.imported_values.contains_key(name) || matches!(prev, Ty::Module(_)))
      || self.import_binds.get(name)
             .is_some_and(|i| (i.line, i.col) < (span.line, span.col)))
  ```

  Imports are **hoisted**, so the checker binds them before every top-level statement whatever line
  they sit on. **Two wrong cuts were measured before this one**, in opposite directions:

  1. *"the name was ever imported"* (`!imported_values.contains_key(name)`) — rejects the *sound*
     `x := 1` / `f := fn() -> int: x` / `import COUNT as x from lib` (the hoisted import lands first at
     runtime, so `x` is `1` from line 1 on; measured `1` pre-change and now), **and** lets the headline
     defect through one `import` away (`import COUNT from lib` / closure / `COUNT := "s"` was
     byte-identical pre/post; now rejected).
  2. *the span, consulted unconditionally* — turns a source-**later** import into a per-**name**
     suppressor: `x := 1` / `f := fn() -> int: x` / `x := "s"` / `import COUNT as x from lib` was
     check-clean and printed `s` out of a `fn() -> int`, i.e. `W7-42` verbatim. By that second `let`
     the name had already been handed back (`declare` clears `imported_values`, `setup.rs:1764-1766`),
     so `prev` was the first *let's* `int` and where a later `import` sits was simply not the question.
     The control confirms it: the identical program whose later import binds a **different** name
     always fired.

  Hence the span is consulted only while `prev` really is the import's binding, which
  `imported_values` / `Ty::Module` already say. **The comparison is total, with no tie case:** imports
  are syntactically top-level only (no nested-scope case) and the language has no statement separator,
  so two top-level statements can never share a position — a `<=` tie-break would be dead code.

The error still calls `self.declare(...)` rather than returning early, so the from-import hand-back
(`setup.rs:1764-1766`, which makes `import COUNT from lib.st` / `COUNT := COUNT + 1` legal) survives.

**The hand-back survives only at the SAME type — a real, deliberate cost.**
`import COUNT from lib` (a `str`) / `COUNT := 0` / `COUNT = COUNT + 1` now rejects `str -> int`, and
**annotating does not help** (`COUNT: int = 0` has the same `declared`); the escape is a rename. It has
to be: the import's binding *is* the slot's first declaration, so everything typed against the imported
`str` — including a `fn` that writes it — is still typed that way. Both pre-existing hand-back fixtures
are `int -> int`, so the suite was structurally blind to the type-changing form; it is now pinned as
deliberate by `from_import_hand_back_at_a_different_type_rejected_deliberately`.

**`int` → `float` is rejected too.** Measured pre-fix: `x := 1` / `f := fn() -> int: x` / `x := 1.0`
gave `y: int = f()` the value `1.0` and `print(y + 1)` printed `2.0` — the identical unsoundness, so
the implicit-widening direction gets no exemption.

**A NARROWING is rejected too, deliberately.** `v: Any = 1` / `v := "s"`, `s: Shape = Circle(1)` /
`s := Circle(2)`, and `xs: List[Any] = [1]` / `xs := ["a"]` all reject, even though a `fn() -> Any`
handing back a `str` is no lie. The predicate is on the **slot**, not on its readers: the declared type
is frozen at the first declaration, so an earlier *writer* stays typed against the old one. Measured
pre-fix, check-clean and dead at runtime:

```chezzi
v: Any = 1
fn setv():
    v = 42
v := "s"
setv()
y: str = v
print(y.upper())   # runtime error: type int has no method 'upper'
```

Loosening the test to `assignable` would re-admit exactly that, so `prev != declared` stands. The
escape the diagnostic names covers it: **re-annotating** is accepted (`v: Any = "s"`,
`s: Shape = Circle(2)`), because the annotation restates the frozen type instead of changing it.

**Evidence.** A **36-program before/after table** run on the saved pre-change release binary and the
post-change one (the primary deliverable — a green suite proves nothing about a widening, per the two
fully-green regressions this month). **23 of 36 rows keep their exact verdict *and* their
byte-identical runtime output**; the 13 that change verdict are all module-slot retypes — the four
original shapes (`int -> str` via `:=`, the same via typed let, `int -> float`, `List[int] ->
List[str]`), the four import-order shapes now closed (a from-import value and a whole-module bind, each
with and without a live reader), and the four narrowing shapes above. Plus a `chezzi check` sweep of
all **422** shipped `.chz` in `examples/ std/ benches/ tests/`: **0** hits of the new diagnostic.
Tests: 13 checker tests in `src/checker/tests.rs` (the rejects, the `ok` neighbours, the
const-precedence check, the no-double-fire-under-inferred-return check, the hoisted-import `ok` in
**both** source orders, and the deliberate-narrowing family with its escape) plus
`tests/chz/spec/module_scope_redeclare_test.chz` for the runtime half on both engines.
`docs/reference/grammar.bnf` needed no change — the **syntax** of `:=` is unchanged; this is a checker rule.

#### W7-42r — four module-global retype shapes the `W7-42` rule does not reach — **FIXED (2026-08-14)**

> **Verdict: all four shapes closed, plus an arity hole and an over-rejection found while doing it.**
> Every rejected program was first re-measured on the pre-change binary (`ok: no type errors`, then the
> wrong-typed value or the fault) — "the diagnostic appears" was never accepted as evidence. The design
> that shipped is **symmetric in source position for the fn namespace** and gated on **readers**, not on
> where the `fn` sits; the directional design this row originally implied was measured wrong (see below).
> `cargo test` 4348 passed / 0 failed, clippy clean, and a 435-file corpus sweep pre-vs-post shows
> **0 verdict changes**.


The fix covers the single-name `let`/`:=` binding site whose previous binding is a VALUE in `scopes[0]`.
Four shapes are still check-clean, all the same class:

```chezzi
# (a) DESTRUCTURING — sig.rs:2863-2894
x := 1
f := fn() -> int: x
x, y := ("s", 2)
print(f())      # s      ← a str out of a fn declared `-> int`, check-clean
z: int = f()    # accepted
```

```chezzi
# (b) a FORWARD read of a hoisted import, retyped by a source-EARLIER let — setup.rs:1126, :1341
# lib.chz: COUNT := "sv"
f := fn() -> str: x     # `x` resolves to the HOISTED import on line 3
x := 1
import COUNT as x from lib
print(f())              # 1   ← an int out of a fn declared `-> str`, check-clean
```

```chezzi
# (c) a from-imported FN, retyped — outside the rule because it never lands in `scopes[0]`
# lib.chz: fn g() -> str: return "m"
import g from lib
f := fn() -> str: g()
g := 1
print(f())              # runtime error: 'int' is not callable — check-clean on BOTH binaries
```

```chezzi
# (d) a SAME-MODULE top-level fn, retyped — the commoner shape of (c), and the same cause
fn helper() -> int:
    return 7
f := fn() -> int: helper()
helper := 3
print(f())              # runtime error: 'int' is not callable — check-clean
```

All four were measured on the post-fix binary; (c) and (d) are identical on the pre-change binary too,
so neither is a regression. **(d)'s cause:** the rule reads `prev` from `self.scopes[0]`, but a
top-level `fn` is recorded in `Checker::functions` (`src/checker/mod.rs:634`) — a **disjoint** namespace
— so the lookup returns `None` and the rule cannot fire. It was *assessed and deliberately not fixed in
the same commit as the `known()` repair*: consulting `functions` for `prev` opens a **new reject
family** the existing 80-cell import sweep does not cover a single cell of, and two measured legal
neighbours already sit inside it — `fn helper() -> int: …` then `helper := fn() -> int: 9` (runs, prints
`9`) and `helper := 3` **before** `fn helper() …` (runs, prints `3`). The second is the trap: `functions`
is populated by the **hoist**, so it is source-position-blind, exactly the axis that produced two wrong
cuts of the import half of `W7-42`. A correct fix needs a per-fn declaration-span map (the `import_binds`
analogue), a decision on `Ty::Func` equality (`merge_unknown` has no `Func` arm, so any signature
difference would fire), and its own sweep before it can be trusted. **(b) is the remaining half of the import case, not the whole of it:** the source-*earlier*-`import` shape (`import COUNT from lib` / a closure / `COUNT := "s"`)
**is now rejected** by the `W7-42` rule's span comparison. What is left is the *forward* read — a
closure written **above** the `import` that nonetheless resolves against it, which the `let` between
them then retypes. That is not a `declare`-caller bug at all: it is the checker letting a source line
read a binding the hoist created below it, so the fix belongs with forward-reference resolution, not
with a second copy of the `W7-42` predicate. Top-level `for` and `match` binds are **not** affected:
both push a scope first (measured — `x := "s"` then `for x in 0..3:` leaves `x` as `"s"` after the
loop, and a top-level `for` over a `List[str]` after `x := 1` leaves a pre-existing `fn() -> int`
closure reading `1`).

### W7-43 — `?.` / `??` on a `Result` emits three errors naming variants the user never wrote — **FIXED (2026-08-11)**

> **Verdict: the FULL fix shipped — `?.` on a `Result` now MEANS `?` then `.`.** The whitespace cliff is
> gone, not just the diagnostics. Rust owns enums/errors/control flow (CLAUDE.md) and `f()?.len()`
> compiles in Rust; a language whose `?` propagates the `Err` and whose `.` reads the value had no
> defensible reason to reject their concatenation over one absent space.

**The single most-hit papercut in the external review** — four separate testers hit it, one of them four
times in 30 programs.

**The repro, and the before/after, both measured on the release binary.**

```chezzi
fn f() -> Result[str, str]:
    return Ok("hi")
fn main() -> Result[int, str]:
    n := f()?.len()
```

| | `chezzi check` |
|---|---|
| **BEFORE** | `type error (line 4, col 10): 'Some' is not a variant of Result`<br>`type error (line 4, col 10): 'None' is not a variant of Result`<br>`type error (line 4, col 10): non-exhaustive match on Result: missing Err, Ok` |
| **AFTER** | `ok: no type errors` |

Three errors for one non-mistake, and **none of them named `?.`**. `f()? .len()` — one space — always
worked; that is now the *same program*, measured on both engines:

| program | `chezzi run` (M:N) | `chezzi run --serial` |
|---|---|---|
| `f() -> Ok("hi")`, `n := f()?.len()` | `Ok(2)` rc=0 | `Ok(2)` rc=0 |
| same, spelled `f()? .len()` | `Ok(2)` rc=0 | `Ok(2)` rc=0 |
| `f() -> Err("boom")`, `n := f()?.len()` | `Err('boom')` rc=0 | `Err('boom')` rc=0 |
| same, spelled `f()? .len()` | `Err('boom')` rc=0 | `Err('boom')` rc=0 |

**Cause (what it was).** The lexer merges `?.` when adjacent, type-blindly
(`src/lexer/mod.rs:819-829`). `Desugar::lower_carrier` then lowered it to a `Match` with hard-coded
`Some`/`None` arms **before any typing** (desugar runs from `resolver::build_graph`), so the checker
only ever saw a `Match` on a `Result` with `Some`/`None` arms and reported accordingly — all three
diagnostics on the `OptChain` span, because the whole `Match` carried it.

**Mechanism of the fix — the carriers survive desugar, exactly as `ExprKind::Try` always has.**
`OptChain`/`NullCoalesce` are no longer lowered by desugar (which now does a plain structural walk of
their children plus `normalize_opt_call`, so named args, omitted defaults and variadics on a `?.`
method call are still bound); the two `unreachable!()`s asserting the node was dead are retired. The
**checker** types the operand, picks Option-vs-Try, then **clone-lowers to a real AST shape and infers
THAT** — `Match{Some/None}` for an `Option`, `Field`/`Call` over `Try(obj)` for a `Result`.

That last detail is why **the row's own predicted cost centre cost nothing**. The row warned that the
full version "additionally needs `infer_try`'s enclosing-fn-return-kind gate and its `recover:`/`defer:`
cases … **that interaction is the cost centre**". Because the Result-mode clone literally *contains* an
`ExprKind::Try`, all three of `infer_try`'s gates — `recover_depth`, `in_defer_block`, and the
`current_ret`/`in_fn_body` return-kind gate — fire at the right nesting with **zero new gate code**. The
cost centre was discharged **by construction**, not by re-implementing it.

**The `CarrierTable`, and why it is keyed on `name_span` and not on the node's span.** The checker's
choice is recorded in `(module_idx, frag_ctx, frag_ord, name_span)`; the compiler looks it up and
clone-lowers with **the same shared functions** (`desugar::lower_carrier_option` /
`lower_carrier_try`), so spans, table keys and bytecode cannot drift between the two halves. A
**missing** entry is a hard `CompileError`, never a default — absent means the halves disagree about
the program, and defaulting would turn that into a wrong runtime value shape under a green
`chezzi check`.

`name_span` is a **new field on `ExprKind::OptChain`** and it is load-bearing: `parse_postfix`
(`src/parser/mod.rs:2378`) takes `let span = e.span;` **once, before the postfix match**, so every link
of `a?.b?.c` carries the span of the primary `a`. A **mixed chain** — `a: Result`, `a.b: Option` — has
two links with **different modes**, so keying on the node span would alias them onto one entry, the
later insert would win, and the compiler would emit the wrong lowering for one link under a green
`chezzi check`. A silent wrong value. The `?.`-following name token is the only per-link source
position a chain has. (Free side-fix: `lower_carrier_option`'s synthesized `Field` now also uses the
carrier's real `name_span`, so `a?.m(c)?.n(c)` no longer collides two distinct witness calls on one
`WitnessKey`.)

**`??` stays `Option`-only — deliberately, and now with ONE accurate error.** **SUPERSEDED 2026-09-02 (TICKET-039): `??` now also accepts a `Result`, discarding the error payload, on the owner's decision.** The "no ancestor supports it" reason below was the one measured wrong — Rust has no coalescing *operator*, but it does have the same explicit discard as a method (`r.unwrap_or(0)` compiles and yields `0`), so the ancestor supports the CAPABILITY and only the spelling differs. The rest of this paragraph and its measured error are the historical record of the decision this reversed. Reasons as filed: it has no
spaced alternative spelling, so there was no whitespace trap to remove — only a new operator meaning to
invent; no ancestor supports it (Rust deliberately has no coalescing operator; Swift/Kotlin/C#'s is
Optional-only, in languages with no `Result`); and it would silently discard an error payload, which
contradicts `src/checker/pattern.rs`'s must-handle stance and `W7-46`. Measured:

```
type error (line 4, col 10): '??' applies to an Option, found Result[str, str] — a Result carries an
error that must be handled: use a match with Ok/Err arms
```

Two more measured one-error verdicts: `?.` on a non-carrier →
`'?.' applies to an Option or a Result, found int`; and the **migration papercut**,
`f()?.len() ?? 0` on a `Result` → `'??' applies to an Option, found int` (the `?.` already unwrapped to
`int`; drop the `?? 0`, or `match` if you want a fallback instead of propagation).

**`?.` on a `Result` is try-then-DOT, not a chain of tries.** It does not auto-try through a
`Result[Option[T]]` payload: with `a: Option`, `a.b: Result`, `a?.b?.c` is an `Option[Result[T, E]]`
followed by a field access on a `Result` — an error, and correctly so.

**Rejected alternative — a runtime-dispatched opcode pair.** The VM already branches on
`VID_OK`/`VID_ERR`/`VID_SOME`/`VID_NONE_VARIANT` in `Vm::do_try` (`src/vm/stmt.rs:120`), so a
`CarrierEnter`/`CarrierExit` pair could have decided at runtime. Cost: two opcodes, two new paths that
must stay serial==M:N-identical, plus a runtime "was it an Option" marker so the exit knows whether to
re-wrap. Benefit: **zero** — the checker must decide statically anyway, because the expression's type
is `T` vs `Option[T]` and every downstream node is compiled against it. Recording a decision the
checker is already forced to make is one HashMap insert. **The VM is unchanged**: the Result path emits
`Op::Try` followed by the same field/method opcodes the spaced form emits — literally the same program.

**Evidence.** 3991 lib tests green. `tests/chz/spec/opt_carrier_test.chz` (21 `test fn`, 450 chz tests
in the suite) passes on **both** engines under `chz_suite_passes`; every case is an
**equivalence** assertion between the `?.` and `? .` spellings, which is what makes a *widening*
testable at all (project memory: "a widening is untested by its own suite" — the pre-change suite could
not have an opinion about a program it rejected). The load-bearing Rust test compiles `f()?.len()` and
`f()? .len()` in the same fn and asserts the **opcode sequences are identical** (and, on
column-aligned sources, the spans too) — it checks the *program produced*, not that a branch fired.
`docs/reference/syntax.md` §9 and `docs/reference/grammar.bnf`'s two carrier comments moved with it; the grammar
**productions did not change**, so `cargo test conformance` stayed green without a production edit.

**Residual, since FIXED 2026-08-12: `W7-48`** — a `?` (and therefore a `?.`) inside a `spawn:` block
was checked against the **enclosing fn's** return kind, not the task's. It is now a compile error.

### W7-44 — the documented secrets surface is dead off Linux, and `uuid.v4()` is not a CSPRNG on ANY platform — **FIXED (2026-08-14)**

> **Verdict: both halves closed, and the security property was PROVEN rather than asserted.** A
> state-recovery attack against the pre-change binary recovers the SplitMix64 state from **one** observed
> unseeded UUID and predicts the next byte-exactly — and against the repo's own frozen seed-42 vector it
> recovers `0x9e3779b97f4a7c3f`, i.e. seed 42 + GOLDEN. The same attack **fails 10/10** against the
> post-change binary, while `uuid_seed(42)` still reproduces the frozen vector byte-for-byte. Windows was
> deliberately left fail-closed rather than shipping a `BCryptGenRandom` arm no one here can compile or
> run (CI is ubuntu-only; `cffi` is already `#[cfg(unix)]`-gated) — an unverifiable arm on an unsupported
> target is not a fix.


**Two defects, one module family.**

1. **`crypto.secure_bytes` / `crypto.token_hex` are dead on macOS and Windows.**
   `secure_random_bytes` (`src/native/crypto.rs:514`) has a
   `#[cfg(not(target_os = "linux"))]` arm (`:547-553`) returning
   `Err("secure_bytes: no secure entropy source on this platform")`. That is the whole documented
   `secrets` equivalent, with **no caveat anywhere in the docs**. The fail-closed choice is right; the
   missing platform wiring and the missing doc caveat are not.
2. **`uuid.v4()` is predictable from ONE observed output, which also recovers the SEED — and this is
   true on Linux too** (stronger than the external report, which only flagged the non-Linux seed, and
   stronger than this row's own first cut, which said "two"). `v4` (`src/native/uuid.rs:74`) is
   `with_state(|s| (next_u64(s), next_u64(s)))` over a **64-bit SplitMix64** state. SplitMix64's output
   function is a **bijection** and `v4` overwrites only **6** of the 128 drawn bits (4 version, 2
   variant), so inverting the first `u64` over the 16 version candidates and filtering on the second's
   surviving 62 bits leaves **exactly one** state — *measured against this repo's own frozen seed-42
   vector* (`src/native/uuid.rs:191`): it yields `0x9e3779b97f4a7c3f` = seed 42 + GOLDEN, recovers the
   seed `42`, and predicts UUID #2 byte-exactly. With the seed in hand, *earlier* UUIDs are recoverable
   too. Off Linux the seed is additionally just `nanos ^ addr` (`uuid.rs:41-47`).
   **`std.rand` has the identical property** — `uuid` draws through `super::rand::next_u64`, and
   `rand`'s own `auto_seed` (`src/native/rand.rs:47-70`) is the same `cfg(target_os = "linux")`-gated
   `getrandom` with a nanos ^ addr ^ counter fallback — so it carries to `iter.shuffle`/`choice`/
   `sample`, which call `rand.int` (`std/iter.chz:225,243`). `docs/reference/stdlib.md:952` says the stream is *"auto-seeded from OS entropy"* with no
   platform caveat and **no "not for security" warning**, while `:943-945` already warns correctly about
   MD5/SHA-1 — the file has the right shape to copy.

**Split the work.**

- **(a) Docs, cheap, should not wait — LANDED 2026-08-10, commit `docs(stdlib): the CSPRNG surface
  names its Linux-only wiring, and uuid.v4 says it is not secure`:** an explicit *"`uuid.v4()` is NOT
  cryptographically secure — use `crypto.token_hex` for secrets"* warning, plus the Linux-only caveat
  on `crypto.secure_bytes`/`token_hex`. **Amended the same day (adversarial review):** the first cut
  said *two* observed UUIDs and *state* recovery — both understatements, and an understated security
  caveat is worse than none. It now says **one**, and says it recovers the **seed**. The same pass gave
  `std.rand` the platform caveat and the not-for-crypto warning it had been missing all along (the
  identical blanket claim, two sections above the one this row corrected), pointed `iter.shuffle`/
  `choice`/`sample` at it, and stopped the `crypto` platform line's "macOS and Windows" from reading as
  an exhaustive enumeration of `not(target_os = "linux")`.
- ~~**(b) Its own task (the code fix — portable entropy)**~~ — **DONE 2026-08-14**, and this bullet's
  own plan is what shipped, minus Windows. `crypto::os_entropy` is now the single source: Linux
  `getrandom(2)` → `/dev/urandom` on any other unix (macOS/BSD) → fail-closed on non-unix, and an
  unseeded `uuid.v4()` draws 16 fresh bytes from it per call rather than stepping a seeded PRNG.
  `BCryptGenRandom` was deliberately NOT wired (no cross-target toolchain here, CI is ubuntu-only,
  `cffi` is already `#[cfg(unix)]`-gated) — Windows keeps the fail-closed fault. See the verdict block
  at the top of this section for the measured proof.

**Ancestor, measured:** Python's `secrets` and `uuid.uuid4()` both draw from `os.urandom` on **every**
platform; Go's `crypto/rand` likewise. Neither ships a 64-bit-state UUID. Chezzi now matches on unix.

### W7-45 — the `where`-bound hole `W7-41` closed for `==` was still open on every builtin whose runtime is `values_equal` — **FIXED 2026-08-11**

**The bug, confirmed by RUNNING on the pre-change binary — all `ok: no type errors` then a runtime
fault**, with `W7-41`'s `struct Box[T]` carrying `fn eq(...) where T: Comparable` over a
non-`Comparable` `Tag` (and a constant `hash`), `a = Box(Tag(1))`, `b = Box(Tag(2))`:

| expression | pre-change binary | post-change `chezzi check` |
|---|---|---|
| `[a].contains(b)` | `runtime error (line 7, col 12): struct 'Tag' has no 'compare' method` | *contains() compares List[Box[Tag]] elements for equality — Box[Tag]'s `eq` requires Tag: Comparable* |
| `[a].index_of(b)` | same fault | same shape, `index_of()` |
| `[a, b].dedup()` | same fault | same shape, `dedup()` |
| `[a, b].unique()` | same fault — **missing from this row as first filed** | same shape, `unique()` |
| `a in [b]` | same fault | *cannot test membership of Box[Tag] in List[Box[Tag]] — Box[Tag]'s `eq` requires Tag: Comparable* |
| `{a, b}` set literal | same fault | *set element type Box[Tag]'s `eq` requires Tag: Comparable* (one per element, as rustc does) |
| `Set([a, b])` | same fault | *Set element type Box[Tag]'s `eq` requires Tag: Comparable* |
| `{a: 1}` map key | **prints `1`, rc=0** — a single key never collides, so no `eq` ever runs | *map key type Box[Tag]'s `eq` requires Tag: Comparable* |

That last row is the point: for `Set`/`Map` the hook dispatches **only on a hash collision**, so the
*same* unsound program's verdict depended on hash values — fault with a forced collision, silent wrong
answer without. **The silent-wrong-answer variant was the worse half of this row.**

**Cause.** These have a runtime of `values_equal` but a checker signature validated only by
`assignable`, so `may_be_equal` — and therefore `W7-41`'s guard — was never consulted.

**The fix, two commits, the second one line.**

- `caaa5fcc` — the four `List` methods are gated at the `Ty::List` dispatch arm
  (`src/checker/expr.rs:2754`), beside `sum`'s numeric gate, which is the exact precedent for a
  dispatch-time element gate. `List` puts **no bound on `T`**, so there is no construction choke point
  and per-method wiring is unavoidable here. Same commit adds the `in`-over-a-list arm
  (`src/checker/pattern.rs:2527`).
- `68f916fc` — every map-key and set-element position funnels through one predicate, so the `Eq`
  conjunct went in there: `is_hashable_key` is now `Checker::key_ty_reject`, with **10 call sites**
  (`expr.rs:1581`, `:1667`; `proto.rs:843`; `pattern.rs:2119`, `:2148`, `:2216`, `:2227`; `sig.rs:1624`,
  `:1631`, `:3049`). That is the Rust ancestor verbatim — `impl<T: Eq + Hash> HashSet<T>` — and
  `Hashable` does not embed `Eq`, so it cannot double-report. The helper returns the message *tail*, so
  all four existing prefixes ("map key type", "set element type", "Set element type", "Map key type")
  survive unchanged and no existing test needle moved.
- `ef3995cb` — a comment-only follow-up recording **why the second conjunct is
  `eq_bounds_unsatisfied_erased` and not `satisfies(t, "Eq")`**, which is what the Rust bound would be.
  The reason is the escape hatch: `struct Holder[T]` with a non-hook `fn eq[U](self, o: U)` works as a
  `Set` element (rc=0 before and after) but FAILS `satisfies(_, "Eq")`, because the structural check
  sees a wrong-signature hook — the exact regression class that got the 2026-08-10 attempt reverted.
  Nothing unsound slips through the weaker conjunct: `values_equal` can only fault on a declared HOOK
  `eq`, which is precisely what `eq_bounds_unsatisfied` detects. The comment says **DO NOT CORRECT THIS
  TO MATCH RUST** and carries the program, because a future reader who "fixes" it silently re-breaks
  `Set[Holder]`.

**Exhaustiveness was proven, two independent ways, rather than asserted.** Against
`std/prelude.chz`'s `native struct List[T]`: of its methods only `contains`/`index_of`/`dedup`/`unique`
take a bare `T` that can reach `values_equal` — `count` and `position` take a **predicate**
(`fn(T) -> bool`), and there is no `remove` (it is `remove_at`, by index). And against the VM's
`elem_equal` call graph: `src/vm/arith.rs:1777` and `src/vm/call.rs:3184` are the only List-side callers.

**Why one line covers the whole Map/Set half.** 18 seams were measured check-clean on the pre-change
binary → rejected after: both `Set` ctor spellings, the bare `{a, b}` literal, set and map
comprehensions, `Set[…]`/`Map[…,…]` annotations, set algebra (`union`/`intersection`/`difference`),
`Map.remove`/`merge`, `m[k]` read and `m[k] = v` write, `refine_receiver`'s late-concrete element
(`s := Set([])` then `s.add(a)`), a struct field / a param / a return type so typed, and
`concurrency.RwShared`/`Shared` over such a container. The three routes that could **obtain** a bad
`Set`/`Map` without touching a literal or a ctor — annotation, fn return type, struct field type —
all reject too, so every SPELLED position is covered by the one choke point rather than gated one by
one.

> **CORRECTION (2026-08-11).** This row previously said the offending type becomes *unspellable* and
> its methods unreachable **by construction**. That is FALSE and the wording is withdrawn. A generic
> factory never spells the element type — its body is checked once with `T` abstract — so
> `fn mk[T: Hashable](x: T) -> Set[T]` returns a `Set[Cond[Tag]]` that no gate saw, and
> `Cond(Tag(2)) in s` on it is check-clean (measured on both the pre-change and post-change binaries;
> it is not a regression, and it is the same erased-generic route filed as **W7-53**'s third
> instance, **since FIXED 2026-08-12** — `[T: Hashable]` alone now rejects at `mk`'s OWN declaration
> unless `Eq` is spelled too, `[T: Hashable + Eq]`, closing the route at the DEFINITION rather than by
> widening this `in` arm). Extending the `in` arm to `Ty::Set` would still close it a second way — at
> the cost of a second diagnostic on the ordinary spelled `x in Set([...])` — but is no longer needed
> now that the declaration itself is judged; that trade stays declined. A choke point covers the
> positions it sees, which is not the same as a proof.

**Corrections to this row as first filed, each re-measured.**

1. **`List.unique` was missing** from the list of affected builtins. It is the same defect.
2. **The `in` arm is typed by `compatible`, not `may_be_equal`** (`src/checker/pattern.rs:2516` for
   list/set, `:2538` for map, `:2559` for the `Contains` protocol). That is exactly why `in` survived
   `W7-41`'s guard by one match arm — the guard went in beside `may_be_equal`, which `in` never calls.
3. **`RwShared` is not a distinct surface.** Its read-view methods are per-container
   (`std/concurrency.chz:34-39`: `RwShared[List[E]]` has `len`/`at`/`slice`/`for_each`/`fold`;
   `RwShared[Map[K,V]]` has `get_key`/`has`/…; `RwShared[Set[E]]` has `contains`/…), and the bad element
   type is refused at the inner `Set`/`Map` construction, so no separate wiring was needed.
4. **`Set([a])` and `Set(xs)` are the SAME checker site** (`expr.rs`'s `"Set"` ctor arm). The site
   genuinely distinct from the ctor is the bare **`{a, b}` set literal** (`pattern.rs::infer_set`).
5. **The surface was far larger than the five expressions filed**: 13 distinct fault fixtures, all
   verified check-clean *and* rc=1 on the pre-change binary before any assertion was written, behind 12
   checker seams (the `Ty::List` method arm, the `in`-over-a-list arm, and `key_ty_reject`'s 10 call
   sites) — plus the 18 subsumed seams above.
6. `List.count` takes a **predicate** (`std/prelude.chz`) so it never routes through `values_equal`, and
   `List.remove` **does not exist** — both non-issues, as originally noted, and both re-confirmed.

**Three traps for anyone re-testing this area** — every one of them produces a false green:

1. The fault needs an `eq` **body that actually uses the bound**. An `eq` doing only
   `self.val == other.val` falls back to structural equality on `Tag` and returns a
   **wrong-but-not-faulting** answer, rc=0.
2. `[a].contains(a)` hits the identity shortcut in `values_equal_guarded` and never dispatches at all —
   use two *distinct* values.
3. Every `Set`/`Map` fixture must use a **constant `hash`**. A payload-derived hash passes rc=0 whether
   or not the bug exists, because no collision means no `eq`.

The Chezzi-side positive control is the other half of the proof:
`tests/chz/spec/eq_protocol_containers_test.chz` runs a `Cond[T]` with a *satisfied* conditional `eq`
and a constant `hash(self) = 7`, so every element collides and the container's answers **are** `eq`'s
answers (`Set([cond(1), cond(2), cond(1)]).len() == 2` is `3` if `eq` never runs). Green on M:N and
`--serial`.

**Residuals:** `W7-52` (protocol existentials) — **since RESOLVED 2026-08-12 (verdict) + FIXED 2026-08-12 (the `[T: Eq]` bound gap)** —
and `W7-53`, **since FIXED 2026-08-12** — the erased call site, whose third instance
(`fn f[T: Hashable](xs: List[T]) -> int: return Set(xs).len()`) was found here.

### W7-46 — a spawned fn's returned `Err` is silently discarded, while a raw fault aborts the nursery — **FIXED (2026-08-10)**

> **Verdict: the semantics are CORRECT and Go-consistent — the defect was the examples.** The ancestors
> were measured (below) before touching anything; the engine was **not** changed.

Found while verifying `W7-40`'s narrowing, and it independently reproduces an item from the same
external review ("spawned `Err` silently swallowed by the nursery — breaks the repo's own
`echo_server_spawn.chz`"). **Confirmed by running.**

**Repro — the repo's own example lies about what it did.** `examples/echo_server.chz` spawns
`acceptor(server, n) -> int!`. On `--serial` every socket op returns the engine `Err` (`W7-40`'s
deliberate divergence), `?` turns each into an `Err` **return value**, the nursery **discards it**,
`run()` returns `Ok(0)`, and the program prints:

```
echo server handled 50 connections
```

rc=0 — having handled **zero**. The example is **vacuously green**, and so is any test asserting that
line.

**The asymmetry is the finding: a raw FAULT in a spawn DOES propagate.** Measured, both engines
identically:

```chezzi
fn boom():
    xs := [1]
    print(xs[99])
fn main():
    spawn:
        boom()
    print("main done")
main()
```

→ `main done`, then `runtime error (line 3, col 11): index 99 out of bounds (len 1)`, **rc=1**. So the
nursery propagates a fault and drops an `Err` value: **its two error channels disagree with each
other.**

**The ancestors, MEASURED (2026-08-10).** `spawn`/`parallel:` is Go's seam, so Go is the owner. Both
programs were run, not reasoned about:

| ancestor | program | stdout / stderr | rc |
|---|---|---|---|
| Go 1.26.5 | `go func(){ _ = work() }()` (`work` returns `errors.New("boom")`), `wg.Wait()` | `main finished normally` | **0** |
| Go 1.26.5 | `go func(){ panic("goroutine blew up") }()`, main sleeps 200 ms | `panic: goroutine blew up` + stack; main's `Println` never runs | **2** — measured on the `go build` binary; `go run` wraps it and exits **1** while printing `exit status 2` |
| Python 3.14.6 | `asyncio.TaskGroup` with one task raising `RuntimeError("boom")` | `ExceptionGroup: unhandled errors in a TaskGroup (1 sub-exception)` | **1** |

**Chezzi matches Go on BOTH channels: value → dropped (rc=0), fault → process dies (rc=1).** So the
asymmetry above is not a disagreement Chezzi invented — it is the Go contract, and it is the reason
`errgroup` exists. Python's `TaskGroup` propagates because its error channel *is* the exception
channel; Chezzi's `Result` is a **value**, and a statement-form `spawn f(x)` has nowhere to put a
returned value, exactly like `go f(x)`. **No engine change.** (Making a spawned `Err` propagate would
also resurrect the per-task outcome value `W7-27` deleted for a measured peak-RSS reason — 339 MB → 45
MB, per that row at `:6971`.)

**What WAS the defect — and what was fixed.** `examples/echo_server.chz` and
`examples/echo_server_spawn.chz` printed a **hard-coded literal** count, so they claimed a success they
did not achieve — the same class as `W7-38`'s "a real bug reported as a non-finding", relocated into
the examples. Both now **count what actually happened**, in the shape `docs/reference/concurrency.md` now
recommends: a shared `AtomicInt` (Go's `atomic.Int64`) that a client bumps only when its own full
round-trip came back `echo:ping`, read after the nursery has joined:

```chezzi
served := AtomicInt(0)
parallel:
    spawn acceptor(server, n)
    for _ in 0..n:
        spawn client(addr, served)
return Ok(served.load())
```

Measured after the fix, all rc=0: `chezzi run examples/echo_server.chz` → `echo server handled 50
connections`; `--serial` → `echo server handled 0 connections` (truthful — every socket op there is
`W7-40`'s engine `Err`). Same for `echo_server_spawn.chz`. Three tests in `src/vm/parity_tests.rs`
asserted the vacuous lines and were given teeth: `echo_server_example_terminates_on_the_serial_engine`
now `assert_eq!`s the honest `0` (the watchdog stays the primary assertion), and
`net_echo_server_services_more_conns_than_workers` / `net_echo_server_spawns_handler_per_connection`
assert `all served: 100` instead of `contains("all served")`. Neither example gains a `.expected`
golden — the ephemeral port keeps them nondeterministic by design.

### W7-47 — `os.exit` does not terminate the process while `main` is blocked in a socket op — **FIXED 2026-08-12**

**Repro.** An eager `Executor` job calls `std.os.exit(3)` while `main` sits in `accept()`:

```chezzi
import std.net
import std.concurrency
import std.os
import std.time

fn bail():
    time.sleep_ms(50)
    print("job calling os.exit")
    os.exit(3)

fn main():
    ex := Executor()
    ex.submit(bail)
    match net.listen("127.0.0.1:8807"):
        Ok(l):
            print("listening")
            print(l.accept())
        Err(e): print("listen err", e.message())

main()
```

**Measured, release binary, default (M:N) engine, under `timeout 12`:**

| | output | rc |
|---|---|---|
| Chezzi | `listening`, `job calling os.exit` | **124 (hang)** |
| Go 1.26.5, same shape (`go func(){ …; os.Exit(3) }()` + `ln.Accept()` on main) | `listening`, `goroutine calling os.Exit` | **3, immediately** |

The job's marker prints, so `os.exit` ran — and then nothing happens. **Go's `os.Exit` is immediate
from any goroutine**, so this is an ancestor divergence on a seam Go owns.

**Cause (pointer, not a fix).** `std.os.exit` inside an eager job does not touch the process: it sets
the worker's `pending_exit` (`src/vm/mod.rs:4137`), which `ReadyWorker::run_outcome`
(`src/vm/mod.rs:3461-3472`) turns into a `TaskOutcome::Exit { code, … }`. That outcome is a value
somebody must **observe** — it is surfaced only when the executor is joined (`shutdown()` /
`join_eager_jobs`, or the program-exit drain). A `main` blocked forever in `accept()` never reaches
either, so the halt sits in a slot nobody reads. `os.exit` from a job is therefore a *deferred* halt
handed up through a join, where Go's is a process-immediate `exit(2)`.

**Pre-existing in NATURE, newly OBSERVABLE.** This is an `os.exit` defect, not something `W7-40`
introduced: the deferred-through-the-join plumbing is unchanged. But before `W7-40` an `accept()` on
top-level `main` returned the immediate `Err("… requires the --parallel engine")`, so `main` could not
block indefinitely and the deferral always resolved. `W7-40` made `main` able to block on a socket at
all, which is what turns "deferred" into "never". Related, same root: `join_eager_jobs` is an untimed
condvar wait (see the smaller-items subsection below), so the two uncancellable waits compound.

**The fix, as landed (2026-08-12).** The run already owns a shared, non-`static` cell:
`Vm::quiesce: Arc<QuiesceState>` (`src/vm/mod.rs:926`), cloned into every worker at `sched.rs:3572`,
and documented at `quiesce.rs:45-49` as per-run precisely because `cargo test` runs many programs
concurrently in ONE process. `request_exit` publishes the masked code there beside the per-`Vm`
`pending_exit` write, and a new `Vm::run_exit_err` lets a blocking loop adopt it: set `pending_exit`,
return the `"exit"` sentinel — verbatim the shape `reduce_task_slots` already produces at
`sched.rs:1934`, so the recover-bypass (`exec.rs:1185/1259/1290`), defer-skipping (`stmt.rs`) and the
driver's `(out, stderr, Ok(()), Some(code))` contract are all unchanged code.

Two things this fix deliberately does NOT do. It does not call **`std::process::exit`** —
`stream.rs:17-20` refuses it (library code for `chezzi-lsp`/embedders; two writer threads racing libc
`exit(3)` is UB; a thread that kills the process discards the run's real outcome), and the CLI's status
comes from *returning* `ExitCode` after `flush_stream()`. And it adds **no wakeup plumbing**: every
wait a counted party can sit in is already a bounded ≤5 ms poll by design (`DEMOTE_POLL_BACKOFF`,
verified in `block_until_deadline`, `block_wait_tick`, the `wait:` block, and `demote_block_socket`'s
`wait_fd_ready(fd, interest, DEMOTE_POLL_BACKOFF)`), so the flag IS the wakeup at the latency
cancellation already pays. `join_eager_jobs`' untimed `eager_cv.wait` is untouched — once each job's
own wait unwinds, `finish()` drains `outstanding` and the join releases through the mechanism it
already has. **Fixing the leaves fixed the join.**

**The rung's position is the design.** In `block_halt_check` the order is `deadline_halt` →
`cancel_requested` → **exit** → `quiesced()`. *Below cancel*, so a party that already holds a cancel
flag keeps unwinding as `Cancelled` exactly as before — only a party with **no cancel flag at all**,
precisely top-level `main` (`sched.rs:1327-1329`), reaches the new rung, which is why
`parallel_os_exit_aborts_recv_blocked_sibling` and `parity_os_exit_inside_a_cancelled_tasks_defer`
did not move. *Above the verdict*, because an `Exit` outranks a synthesized `Deadlocked`.
`demote_block_socket` is the one wait not routed through `block_halt_check` (it hand-rolls its checks,
since a socket-blocked party never registers), so it carries the rung explicitly — as `break Err(e)`,
never `?`, because the loop is bracketed by `demote_socket_enter`/`demote_socket_exit` and returning
past the exit would leak `running`/`inflight` for the rest of the process.

**Measured, release binary, `timeout 12`:**

| program | engine | before | after |
|---|---|---|---|
| `accept()`-blocked `main` | `run` (M:N) | **hang, rc=124** | **rc=3** |
| `accept()`-blocked `main` | `--serial` | rc=3 | rc=3, unchanged |
| `recv()`-blocked `main` | `run` (M:N) | `recv on an empty channel: deadlock`, **rc=1** | **rc=3** |
| `recv()`-blocked `main` | `--serial` | rc=1 | rc=1, unchanged |

`--serial` is bit-for-bit unchanged in both directions: it never blocks on a socket
(`may_block_socket_in_place`) and never sets `eager_core`, so it never reaches the new rung. The repro
is deliberately **not** a parity-gated `.chz` — the engines print different stdout by `W7-40`'s
documented deliberate design, so `assert_file_parity` would fail on that difference rather than on the
bug. The tests live in `tests/exit_status.rs`, the only place asserting a real OS status, each under a
std-only watchdog that **kills and panics** on expiry so a hang fails instead of masking as a pass.

**Two defects caught by adversarial review, both fixed before landing.** Two independent prosecutors
converged on the first; both were reproduced on the release binary before being accepted.

1. **The cell was never cleared, and `chezzi test` reuses ONE `Vm` per file.** `pending_exit` is reset
   per invocation; the new cell was not, so a `test fn` calling `os.exit` poisoned every later test
   that blocked: `test fn a_exits(): os.exit(0)` followed by a `time.sleep_ms(10)` test gave
   `ERROR a_exits … exit` / **`ERROR b_sleeps … exit`**, where the pre-change binary passed `b_sleeps`.
   Order-dependent and identical on both engines, so `chz_suite_passes` was structurally
   blind to it. Cleared in `Vm::invoke_test` beside `reset_over_memory()` — the documented per-test
   reset point — and pinned by `a_test_fn_that_exits_does_not_poison_later_blocking_tests`.
2. **`reduce_task_slots` read `first_exit` from task SLOTS only.** An eager `Executor` job owns no
   slot, so its exit was invisible to a nursery's reduce and the nursery reported `deadlock` while an
   exit was already published — a *confident wrong verdict*, the `parked-is-not-stuck` class, and worse
   than the hang it replaced. Fixed uniformly (`first_exit.or_else(|| self.quiesce.pending())`) rather
   than by special-casing the deadlock arm; **no existing exit test's expected value moved.** Pinned by
   `eager_job_os_exit_beats_a_blocked_nurserys_deadlock_verdict`, whose live sibling is load-bearing,
   not decoration: with the fold-in disabled and rebuilt, the same program reports rc=1 `deadlock`.

**Generalisable lesson: publishing a signal run-wide gives it a LIFETIME, and a lifetime needs an
owner.** Both defects are the same shape — the cell outliving the run it describes (defect 1) and the
cell being invisible to a reducer that only looked at its own slots (defect 2).

### W7-48 — a `?` inside `spawn:` is checked against the ENCLOSING fn's return kind, so its `Err` is accepted and then silently dropped — **FIXED 2026-08-12**

Found while landing `W7-43` (the `?.` carrier work); **pre-existing, and `?.` inherits it identically**
because a Result-mode `?.` lowers to a real `ExprKind::Try`. Not caused by `W7-43`.

**`SpawnTarget::Block` (`src/checker/sig.rs:2674-2710`) resets `in_defer_block` and `loop_depth`
across the task boundary but NOT `current_ret` / `in_fn_body`.** Its own comment (`:2693-2696`) states
the invariant it does not enforce:

> a `?` in this task targets the task (nil-returning → reject), exactly as a bare `spawn: v := g()?`
> does.

That is **false whenever the enclosing fn returns a carrier.** Measured on the release binary,
`chezzi check`:

| enclosing fn | `spawn: v := g()?` where `g() -> Result[int, str]` | verdict |
|---|---|---|
| `fn main():` (nil) | `type error: '?' used in a function that returns nil, not Result or Option` | rejected — the comment's claim, but only by luck |
| `fn main() -> Result[int, str]:` | `ok: no type errors` | **accepted** |
| `fn main() -> Option[int]:` | `type error: '?' propagates a Result error, but the enclosing function returns Option, not Result` | rejected **for the wrong reason** — the diagnostic names the *enclosing function*, which is the bug in one sentence |
| module top level (no fn) | `ok: no type errors` | accepted |

**The accepted case is a silent swallow, not a harmless over-acceptance.** The checker validated the
`?` against `main`'s `-> Result[int, str]`, but at runtime the `spawn` body is its **own** frame, so
the `Err` early-returns out of the *task* — where the nursery discards a returned `Err` by design
(Go's contract, measured in `W7-46`). Measured, both engines identical:

```chezzi
fn g() -> Result[int, str]:
    return Err("boom")
fn main() -> Result[int, str]:
    parallel:
        spawn:
            v := g()?
            print("never {v}")
    print("after nursery")
    return Ok(0)
print(main())
```

→ `after nursery`, `Ok(0)`, **rc=0** on both `chezzi run` and `--serial`. `boom` appears nowhere. The
program the checker approved has a propagation path the program that runs does not have.

**Same shape through `?.`** (the `W7-43` spelling), also check-clean and also both engines:
`spawn: n := f()?.len()` inside a `-> Result[int, str]` fn prints the `Ok` value on the happy path and
silently drops the `Err` on the sad one.

**The fix, as landed (2026-08-12).** The direction is the **Go answer — reject**: a spawned task has
nowhere to propagate to, which is also why `errgroup` exists, and `W7-46` already settled that a
spawned `Err` value is *dropped* by design.

`SpawnTarget::Block` now zeroes `in_defer_block` + `recover_depth` at the task boundary and sets a new
`in_spawn_block` flag, cleared again at every nested fn/closure boundary (saved/restored 1:1 beside
`current_ret` in `check_fn_body` / `infer_fn_ret` / `infer_closure`). Its gate in `infer_try` sits
**after** the `recover:`/`defer:` gates and **before** the return-kind match, and that order is the
whole design: a `recover:`/`defer:` OUTSIDE the task has its state zeroed at the boundary, so its `?`
falls through and is rejected; one nested INSIDE the task re-arms from zero, so its own (per-frame,
correct) contract still wins.

> `'?' is not allowed inside a spawn block: a spawned task has no caller to propagate to`

matching the existing `'{kw}' is not allowed inside a spawn block` for `return`/`break`/`continue` —
a `?` *is* a return.

**`current_ret`/`in_fn_body` are deliberately NOT zeroed, and that is the row's own trap.** The
"two-line mechanical version" this section previously recommended *was implemented first*, and it
regressed a neighbouring diagnostic: `check_return` (`src/checker/sig.rs:3366`) reads the same
`current_ret`, so `spawn: return 5` inside `fn main() -> int` emitted a second, FALSE
`function returns nothing, cannot return a value` on top of the correct
`'return' is not allowed inside a spawn block` — a lie about a fn that plainly returns `int`, i.e. the
exact enclosing-fn-naming failure this row exists to remove. Two independent adversarial-review
prosecutors converged on it from different lenses. Since `in_spawn_block` gates `infer_try` *before*
either field is read, the reset bought nothing; dropping it removes the regression outright. Pinned by
`spawn_block_return_does_not_claim_the_enclosing_fn_returns_nothing`. **Generalisable lesson: when a
new flag makes an old state-reset redundant, the reset is not free — it is still read by whoever else
consults that state.**

The gate is also shaped like the two above it: a non-carrier operand keeps
`'?' expects Result or Option, found int` (the spawn message would *hide* the real defect) and an
already-`Unknown` operand does not cascade a second error
(`q_on_a_non_carrier_in_a_spawn_block_still_reports_the_type`).

**Reject-widening blast radius, measured on the PRE-change binary before landing** (the class project
memory flags as structurally untested by the suite that passes today): **exactly 4 sites in all 386
`.chz` files in the repo** — `tests/chz/spec/opt_carrier_test.chz` §9 (`nursery_dot`/`nursery_spaced`),
added by the `W7-43` landing to assert the buggy acceptance. Zero hits in `examples/`, `std/`,
`tests/corpus/`, `benches/`, `tests/chz/{suites,stdlib}`. Rewritten to keep the section's intent
(dot-vs-spaced equivalence inside a nursery) by moving the `?.` into the `parallel:` body, which
legitimately runs in the parent frame; asserted behaviour (`Ok(20)`, dot == spaced) unchanged, green on
both engines.

Six `ok()` neighbours pinned by `q_next_to_a_spawn_block_stays_legal`, each with a real frame to
propagate to: a `?` in a `parallel:` body (parent frame), in a nested `fn` declared inside the task, in
a closure declared inside the task, in a `defer:` inside the task, in a `recover:` inside the task, and
in an argument to the call form `spawn f(g()?)` (arguments evaluate in the parent before the task
starts). `SpawnTarget::Call` never had the hole for that reason.

**Filed, not fixed — a pre-existing neighbour surfaced by the review.** A `?` in a **parameter
default** is checked at both the declaration site and each call site, and reports at the *declaration's*
span: `fn h(n: int = g()?) -> int` called from a nil fn reports **two** errors, both at `h`'s line, with
no `spawn` anywhere in the program. So `spawn: print(h())` now reports the spawn message on a line that
contains no `spawn` (cross-module, on a line of another file). Pre-existing — the misleading span is a
property of how defaults are checked, not of this fix — and the same family as
`span-keyed-table-aliasing`. Not investigated further here.

### Safe-direction observations and smaller items (2026-08-10)

- **FIXED 2026-08-12: `accept_inside_an_executor_job_errs_instead_of_starving_the_pool` was
  order-dependent, not flaky-by-timing-alone.** `src/vm/parity_tests.rs:3361`. It asserted the
  would-block `Err` from an `accept()` in an eager job — which only happens when the backlog is
  **empty** — while spawning a `net_peer` that retries `connect` every 10 ms from *before* the program
  starts. Once `net.listen` binds, a connection can land in the backlog and `accept()` returns `Ok`
  instead, so the test reads `GOT:hi`. The window between `listen()` and the job's `accept()` decides
  it, so the verdict flips on **unrelated** scheduling changes: measured green 2/2 full `--lib` runs on
  clean HEAD, then red 2/2 after `W7-48` added three checker tests **elsewhere in the suite** (4027 →
  4030 tests reshuffles the `RUST_TEST_THREADS=4` interleaving). The peer was never the safety net —
  `run_net_timeout_watchdog`'s 30 s timeout catches the hang this test guards against — so it only
  ever subtracted determinism. Peer removed; 4031 passed / 0 failed. **Generalisable: a test that
  asserts an operation WOULD block must not run a helper that can satisfy it.** Note also the
  investigation trap hit on the way: running the test binary directly out of `target/debug/deps` picked
  a *different* target's artifact (15/15 red) than the one `cargo test --lib` uses (10/10 green) — the
  stale-binary trap, in its `find`-the-newest-artifact spelling. Verify through `cargo`, not by
  globbing `deps/`.

- **INTERMITTENT: `eager_handshake_is_driven_by_wakeups_not_by_the_poll_timeout` failed once under a
  CPU-capped full-suite run (2026-08-11, M24-6 gate).** `src/vm/tests.rs:12593`. Seen ONCE, on
  `cargo test` inside a `systemd-run … -p CPUQuota=200%` scope with `RUST_TEST_THREADS=1`; it then
  passed alone, and the very next full `--no-fail-fast` run in the same scope was green (4009/4009).
  Recorded rather than dismissed because it is the SAME family as the bullet below and because the
  test's whole claim is that it is load-independent: it asserts on a counter
  (`BLOCK_WAITS_SLEPT_WHILE_READY`) documented as **structurally zero** in a fixed build. Two
  candidate holes, neither checked: the counter is a process-global atomic read as a before/after
  delta, and the VM worker pool is never joined (`src/vm/pool.rs:12`), so a lingering worker from an
  earlier test can bump it inside this test's window; or a 2-core CPU quota really can let a
  `wait_timeout_while` tick expire against a notifier that has been descheduled. **Reproduce under a
  CPU cap before touching it.** Unrelated to the M24-6 change, which is lexer-position-only.

- ~~**INTERMITTENT: `accept_inside_an_executor_job_errs_instead_of_starving_the_pool` fails under suite
  load.**~~ **SUPERSEDED — this is the same test as the FIXED 2026-08-12 bullet above, and that bullet
  is the answer.** The two entries contradicted each other for four days (one "FIXED", one "still
  intermittent") purely because this one was written first and never struck; corrected 2026-08-16.
  The cause was never load *per se*: the test asserted a would-block `Err` from `accept()` while a
  helper `net_peer` retried `connect` every 10 ms, so whether the backlog was empty at the `accept()`
  decided the verdict — which made it flip on **unrelated** suite-size changes that reshuffled the
  `RUST_TEST_THREADS=4` interleaving, and "immediately followed another full run" is exactly such a
  reshuffle. Removing the peer fixed it. The three candidate causes guessed here (port reuse, a
  saturated pool, a demote-path timing hole) were all **wrong**; recorded because guessing at a flake's
  mechanism instead of measuring the ordering cost four days.

- ~~**`net.connect` blocks in place on EVERY engine**~~ and ~~**`join_eager_jobs` is an untimed condvar
  wait**~~ — both **FIXED 2026-08-14** as `W7-59` and `W7-60`; see the table rows. Note the second's
  filed premise had already gone stale before it was fixed: `W7-58` replaced the untimed
  `eager_cv.wait` with a bounded poll, so what W7-60 actually closed was the missing `--timeout` and
  cancel rungs, not the condvar. And the first's proposed fix shape — route `connect` through
  `Vm::may_block_socket_in_place` — was measured **wrong** and not taken: it refuses `--serial`, where
  `connect` blocks correctly because the handshake is finished by the kernel, not by a chezzi peer.
- ~~**A bare top-level `spawn:` does not start until end-of-main**~~ — **SUPERSEDED by the
  "bare `spawn:` start time" table row, FIXED 2026-08-14.** The open question below ("is it documented
  deliberate behavior?") was answered *no*, so it was filed as a defect and fixed: a task now starts at
  its `spawn` on M:N, and the same program prints `A SPAWNED B` (Go's order). See `docs/design/roadmap.md`
  §2c1. The observation as originally recorded, for the record:

  ```chezzi
  print("A")
  spawn:
      print("SPAWNED")
  time.sleep_ms(300)
  print("B")
  ```

  → `A`, `B`, `SPAWNED`. The spawned block does not run during a 300 ms sleep; it is deferred to the
  implicit end-of-main join. **Go's `go func()` starts immediately**, so this is a candidate ancestor
  divergence on the seam Go owns. **Not yet investigated: whether it is documented deliberate behavior**
  — grep `docs/reference/concurrency.md` for the nursery-less `spawn` contract before filing it as a defect; if it
  IS documented, it is a documented-behavior note, not a bug. It surfaced because a `spawn`-based TCP
  repro self-deadlocked: `l.accept()` on `main` waits on a dialer queued behind the end-of-main join.
- **Stale doc comment (trivial).** `src/vm/exec.rs:1530`'s comment lists `op_sock_park` as one of the
  cancellation checkpoints. `grep -rn "sock_park\|SockPark" src/` returns **exactly one hit — the comment
  itself**. No such function, opcode or variant exists.
### W7-49 — the three checker→compiler side tables aliased across a default-parameter splice — **FIXED 2026-08-11**

**Was pre-existing on `main` in two SHIPPED tables** (`KeywordTable`, `WitnessTable`); the
`CarrierTable` `W7-43` added inherited it. Found while landing `W7-43`, and **measured**, not reasoned
about. All three forms are now measured — including the witness one, which this section had only
predicted.

**The mechanism.** All three keys are `(graph_module_idx, frag_ctx: Span, frag_ord: usize, key_span:
Span)` (`src/checker/ty.rs:24,43,109`), and `Span` was `{ line, col }` with **no file identity**.
`desugar` splices a callee's default-parameter expression into the **caller's** AST as a clone that
keeps the **defining** module's spans (`src/desugar/mod.rs:351-406`, filled in `normalize_call`),
while the record site builds the key with the **calling** module's index. Two unrelated call sites
that happen to sit at the same `line:col` in two files therefore shared one entry; the later `insert`
won, and the type-blind compiler applied the survivor's decision to both.

**Measured repro — `KeywordTable`, silent wrong value.**

```chezzi
# lib.chz  (two comment lines put `fn f` on line 6)
fn h(a: int, b: int) -> int:
    return a * 100 + b
g := h
fn f(x: int = g(a=7, b=9)) -> int:      # first named-arg VALUE `7` at line 6, col 19
    return x
```
```chezzi
# main.chz
import f from lib
fn h2(a: int, b: int) -> int:
    return a * 100 + b
g := h2
fn probe() -> int:
    vvvvvv := g(b=1, a=2)               # first named-arg VALUE `1` at line 6, col 19
    return vvvvvv
print(f())
print(probe())
```
```
                       BEFORE (19f7696a)      AFTER
chezzi check main.chz  ok (exit 0)            ok (exit 0)
chezzi run   main.chz  709                    709
                       102   ← WRONG          201   ← right
```
`102` is `h2(a=1, b=2)` — **lib's identity permutation applied to main's reversed call**. Both engines
agreed, so parity was blind to it.

**Same shape on `CarrierTable`, measured:** an Option-mode `?.` inside a default-param expression in
`lib.chz` colliding with a Result-mode `?.` at the same `line:col` in `main.chz` made the compiler emit
the **Option** lowering for a **Result** carrier — `check` clean, then `3` and
`runtime error: no match arm for variant 'Ok'`. It now prints `3` / `4`.

**`WitnessTable` — MEASURED 2026-08-11 (this section previously predicted three modes worst-first;
the real one is the MIDDLE one).** `lib.chz`'s `fn f(c: Counter = empty[Counter]())` and `main.chz`'s
`vvvv: Other = empty()` put their CALLEE TOKEN at the same `17:19`. The observed mode is **(2), the
wrong concrete type constructed**: `chezzi check` is clean, then `probe()` builds lib's `Counter` and
faults `no field 's' on Counter(n=7)`. It now prints `7` / `oth`. Mode **(1)**, the wrong `argc` from
`emit_witness_args`, and mode **(3)**, a false `internal: a static-type witness is recorded …`, do
**not** occur here, because both colliding callees are the same one-witness `empty` and both witnesses
resolve to a `Concrete` key — the two shapes that would produce (1)/(3) need colliding callees with
*different* witness counts, or one witnessed and one not. Worth remembering as a general shape: a
predicted-worst-first list is a hypothesis, and the middle entry is a perfectly ordinary answer.

**One-column-shift control, all three.** Widening the main-side binder by one character (so the key
span moves one column) makes `19f7696a` produce the RIGHT answer in every one of the three. That is
what isolates the fault to the KEY rather than to the lowering, and it is why the tests are written
flush-left with the alignment visible in the source.

**Adjacent hole, closed in the same commit.** `KeywordTable` alone had **no missing-entry guard**: the
lookup fell through to a plain `Op::Call(args.len())` that **silently dropped every named argument**,
where the carrier (`:3852-3859`) and witness (`:4548`) lookups both hard-error. That asymmetry is how
a keyword miss became a wrong value instead of a diagnostic. All three keyword lookup sites (call,
`spawn`, `defer`) now go through one `Compiler::keyword_perm`, which errors on a miss. Verified NOT to
over-fire on the one path that deliberately leaves `named` populated with no table entry:
`print(sep=…, end=…)` returns from the `prelude_fn` arm (`src/compiler/mod.rs:~5037`) long before any
of the three sites, and `spawn`/`defer` of a named `print` is already rejected by the checker.

**The fix as built.** `Span` is now `{ line: u32, col: u32, file: u32 }` — 12 bytes. The `file` id is
assigned once at the single production lex seam `resolver::Builder::parse`, `1..n`, with `0` reserved
for synthesized / standalone single-file lexes; an interpolation fragment inherits its enclosing
literal's id (a fragment belongs to its literal's module by definition); `LoadedModule.file` records
it. **The three key tuples and all nine record/lookup sites are unchanged** — the spliced clone already
carried the definer's spans and therefore now carries the definer's file id, so both halves keep
deriving the key from the same AST with no new state to agree about, and nesting composes with **no
stack**. Two invariant tests pin it: `resolver::tests::every_module_gets_a_distinct_nonzero_span_file_id`
(graph-level: non-zero, pairwise distinct, and stamped on that module's own spans — a new resolver lex
path that forgets the stamp fails it) and `lexer::tests::span_stays_twelve_bytes` (`Proto.lines` holds
one `Span` per OPCODE, so the width is hot VM data; the 24-byte intermediate regressed a bench AND
pushed `parser::MAX_DEPTH` and `vm::VM_STACK_BYTES` off their calibrated margins). Re-spanning the
spliced clone stayed **rejected** — a span in this checker *is* a table key, which `5642b55d` and
`67e848a6` each learned the hard way. The full rejected-alternatives list is in the table row.

**Second guard: a value-comparing conflict backstop.** The three record sites go through
`checker::record_call_table_entry`, which refuses to overwrite a key already bound to a **different**
value and reports it as a hard `CompileError` (the harvest pass discards type errors, so the conflict
travels as a fourth return value of `resolve_call_tables`). It compares **VALUES, never presence**: a
same-key/same-value re-insert is the normal, benign case — two call sites omitting the same default
record the same decision — and a presence-checking guard would reject every program with a same-module
default. This is a backstop under an already-injective key, not a substitute for one.

**Residual — CLOSED 2026-08-13 by W7-51, exactly as this row predicted.** The same default spliced
twice into the **same** module kept one set of spans and therefore one key. A default was cloned into
the CALLER's scope, so a caller-side local could shadow the definer's global and the two splices
genuinely resolved differently:

```chezzi
# main.chz — `g` is the global `ab(a, b)` at one site, the local `ba(b, a)` at the other
g := ab
fn probe() -> int:
    g := ba
    return f()          # f's default is `g(a=7, b=9)`
```
Three measured verdicts on that one unchanged program: `19f7696a` printed `709` / **`907`** — silent,
clean `check`, both engines; `b1307258` refused it at runtime with the value-comparing backstop
(`internal: two different keyword-argument decisions were recorded for one source position …`, no
output); and **HEAD prints `709` / `709`**, which is what CPython prints. The fix this row named as
"the real fix" — **compile a default once in its defining module** — is what landed (`W7-51`), and it
also removes the divergence from CPython that was the root: Python evaluates a default at `def` time in
the DEFINING scope, so the caller's `g` can never reach it. The pass-count question filed as `W7-51`
dissolved with it, as predicted.

**The value-comparing backstop is retained as defence in depth**, and its key is now **structurally**
injective rather than injective-by-argument: a non-literal default expression exists exactly once in
the whole program (in its provider), so no two call sites can record conflicting decisions under one
`(module, span)` key at all. The class is unrepresentable, not merely unlikely. Pinned by
`src/vm/tests.rs::a_default_that_a_caller_local_shadows_still_resolves_in_its_definer`, which runs the
byte-identical source above.

**What the evidence does and does not cover.** The corpus contains **zero** cross-module spliced
non-literal defaults, so the 978-run pre/post sweep proved only that nothing REGRESSED — it proves
nothing at all about the fix working. The proof rests on the measured repro above and on the tests:
`src/vm/tests.rs` `a_spliced_default_does_not_alias_the_callers_keyword_call` /
`…_carrier` / `…_witness` (multi-file by nature, so Rust rather than `chezzi test`; each verified
FAILING on a binary built from `19f7696a` and passing now) plus
`a_double_spliced_default_that_resolves_two_ways_is_a_loud_error` for the residual, and
`tests/chz/spec/default_splice_keys_test.chz` for both guards' over-fire modes (the `print(sep=)` path
and the benign same-value re-insert) on both engines.

### W7-54 — a function value does not satisfy `Eq`, though `f == g` works — **FIXED 2026-08-12**

One relation, three spellings, three different answers on the pre-change binary:

```chezzi
fn g(x: int) -> int:
    return x

f := g
print(f == g)        # true          <- correct, matches CPython
print(f.eq(g))       # type error (line 5): type fn(int) -> int has no method 'eq'
```

**That second line — `f.eq(g)` called BARE on a concrete receiver — still errors identically after
this fix, and correctly so.** The `Eq` grant makes `.eq()` callable from an ERASED `[T: Eq]` body, not
a new bare instance method on every granted kind — `(1).eq(1)` and `[1].eq([1])` are (and stay) *has
no method 'eq'* too. So "one answer" below means "operator ≡ the erased-bound `.eq()` spelling", not
"bare `.eq()` starts working"; the actually-fixed spelling is the next block.

```chezzi
fn pick[T: Eq](a: T, b: T) -> T:     # type error: type fn(int) -> int does not satisfy Eq
    ...
print(pick(g, g)(3))
```

And `W7-41` widened it to a fourth spelling: `struct Box[T]` with
`fn eq(self, o: Self) -> bool where T: Eq` over a function payload printed `true` before that
milestone and became *cannot compare Box[fn(int) -> int] and Box[fn(int) -> int] for equality —
Box[fn(int) -> int]'s `eq` requires fn(int) -> int: Eq* after it — a regression, not a new hole.

**Ancestor verdict, measured, not re-derived from `W7-41`'s filing:**

| | verdict |
|---|---|
| CPython 3.14 | `f == g` → `True`; `f.__eq__(g)` → `True`; `mk() == mk()` (two calls to a factory) → `False`; two closures with equal captures → `False`; `r == r` → `True` |
| rustc 1.97.0 | `#[derive(PartialEq, Eq)] struct Boxy<T: Eq>(T)` over a `fn(i32) -> i32` payload **builds clean and prints `true`** — fn pointers implement `PartialEq` |
| Go 1.26 | rejects `f == g` (`func can only be compared to nil`) — the one ancestor that differs |

Chezzi's `==` on function values **already matched CPython exactly** before this fix — measured on
the pre-change binary, both engines: `a := g` / `b := g` gives `a == b` → `true`, `a == a` → `true`,
`g == h` (two different top-level fns) → `false`, `mk() == mk()` (a factory returning a nested fn) →
`false`, `mklam(1) == mklam(1)` (two closures, equal captures) → `false`, `r == r` → `true`. So this
was a **checker gap, not a runtime one** — no change to `Vm::values_equal`/`values_equal_guarded` was
needed or made.

**Root cause.** `Ty::Func` had no arm in `Checker::intrinsic_recv_kind` (`src/checker/proto.rs:310`),
so it fell to the `"?"` catch-all. The `Eq` grant arm (D1, `src/checker/proto.rs:1760`) filters on
`!matches!(Self::intrinsic_recv_kind(ty), "?" | "nil")`, so `Ty::Func` was excluded from the grant
before `eq_bounds_unsatisfied` was ever consulted, and with no protocol grant the structural fallback
reported *does not satisfy Eq* at a `[T: Eq]` bound (and identically at `.eq()`, which is dispatched
through the same erased-bound path) — while the *operator* `==` never asks the protocol system at all,
so it kept answering correctly by IDENTITY the whole time. `eq_bounds_unsatisfied_rec`'s catch-all arm
already documented `Func` as "nothing a user `eq` can hide behind, so there is genuinely nothing to
prove" — it just could never be reached, because the D1 gate turned it away first.

**The fix — three checker-only edits, no VM change:**

1. `Ty::Func { .. } => "func"` added to `Checker::intrinsic_recv_kind` (`src/checker/proto.rs:310`).
2. `("Eq", "eq", "func")` added to `INTRINSIC_PROTO_METHODS` (`src/checker/proto.rs:159`), with the
   group comment corrected: most rows are the structural derive, but `func`'s `==` is IDENTITY, not
   structural — still the same `values_equal_guarded` worker, so `.eq()` can never disagree with `==`.
3. A `"func"` `Recv` row added to the probe table inside `intrinsic_grants_all_have_vm_arms`
   (`src/vm/tests.rs`) — the VM-callability half of the W6-3 ratchet.

**No VM arm was needed, confirmed by running it rather than assumed.** `do_method_call`'s terminal `_`
arm (`src/vm/call.rs:1565`) already routes an `Obj::Func`/`Obj::Closure` receiver miss into
`intrinsic_proto_method`, whose receiver-agnostic `("eq", 1)` arm (`:2541`) calls the same
`values_equal_guarded` the `==` operator uses. `W7-54`'s own filing had left this **UNVERIFIED**
("no well-typed program can reach it today") — the probe added in edit 3 is what settled it, and it
settled it in the direction the filing hoped: green on both engines with no fourth edit.

**The matrix sweep is why the grant is trustworthy, not just callable.**
`intrinsic_grants_all_have_vm_arms` sweeps every registered protocol against every classified receiver
kind and asserts the checker's accepted set equals `INTRINSIC_PROTO_METHODS`. Adding `"func"` put
`Ty::Func` through that full sweep for the first time (every other protocol, not just `Eq`), and it
stayed green — no other protocol arm was accidentally granting `Ty::Func` a conformance it cannot back
with a runtime method.

**Scope fence held.** `Hashable` was deliberately NOT granted to `Ty::Func` — CPython allows a function
as a dict key, but Chezzi already rejects `Set[fn]`/`Map[fn, _]` at check time (a check-time reject,
not a check-OK-then-fault), and that is a different row's work, not this one's.

**Same-day follow-up (still 2026-08-12) — `Ty::BuiltinFn` was the identical defect, missed by the
first pass.** A first-class universe builtin (`ord`/`chr`/`panic`/first-class `print`) types as
`Ty::BuiltinFn`, a DISTINCT `Ty` variant from `Ty::Func` kept apart only so it can be sendable across
`spawn` (`docs/reference/syntax.md`: all four universe builtins are sendable, a plain closure is not) — but it
renders identically ("fn(str) -> int") in every diagnostic, so `Ty::Func { .. } => "func"` alone left
it falling to `"?"` and the D1 grant still excluded it:

```chezzi
fn eqm[T: Eq](a: T, b: T) -> bool:
    return a.eq(b)
a := ord
b := ord
print(a == b)      # true
print(eqm(a, b))   # type error (line 6): type fn(str) -> int does not satisfy Eq
```

Fixed by classifying `Ty::BuiltinFn { .. }` to the SAME `"func"` kind rather than minting a second
one — the runtime already answers both identically (`vm/arith.rs`'s `(Obj::Builtin(a),
Obj::Builtin(b)) => Ok(a == b)` arm, confirmed by running it), so one kind, one row, speaks for both.
`intrinsic_grants_all_have_vm_arms` now carries a **second `Recv` row for the same `"func"` kind**
whose literal is `ord`, so the matrix genuinely feeds a `Ty::BuiltinFn` to every protocol's
`bound_probe` and the "no other protocol arm grants it" claim is asserted rather than assumed. (The
first version of this row shipped without that second row and the sentence was false-by-construction:
the sweep only ever saw the `Ty::Func` literal `g`. Caught in review. The invariant held — 17
protocols probed by hand against `ord`, only `Eq` granted — but "the sweep stayed green" was not
evidence for it, which is exactly the ratchet-culture distinction this file exists to keep.) Post-fix,
both `print` statements above output `true`.

**Tests.** `tests/chz/spec/eq_func_test.chz` (10 tests, gated serial==M:N by
`test_runner::chz_suite_passes`): two top-level loads of one `fn` equal (`==` and the
erased `[T: Eq]`/`.eq()` spelling both agree); two different top-level fns unequal; two calls to a
factory returning a nested `fn` unequal; two closures with equal captures unequal; a closure equal to
itself; the `W7-41`-regressed `Box[fn]` spelling restored to `true` (its `eq` body now dispatches a
REAL `self.val == o.val` rather than a `return true` stub, so `Box(g) == Box(h)` → `false` proves the
user method actually ran, not just that the two `Box` handles are structurally equal); and the same
`==`/erased-`.eq()`/`Box[T]` shapes over a builtin (`ord`, `chr`). Plus
`checker::tests::function_value_satisfies_eq_by_identity` and
`checker::tests::builtin_function_value_satisfies_eq_by_identity` (the `[T: Eq]`-fed-a-function and
-a-builtin cases) and the known-gap test at
`checker::tests::eq_operator_enforces_the_receivers_eq_where_bounds` updated from `entry_rejects` to
`entry_ok` for the function-payload `Box[T]` row (it now pins the fix, not the bug; `Box[Channel]`
stays rejected, per rustc). Every new/changed assertion was run against the pre-change binary first
and measured failing with the exact messages quoted above.

### W7-52 — comparing a protocol-typed value defers to the witness at runtime — **RESOLVED 2026-08-12 (verdict), FIXED 2026-08-12 (the `[T: Eq]` bound gap the verdict left in place)**

**AMENDED 2026-09-04 (TICKET-053):** the "defers to the witness at runtime" verdict below held only
for a witness with no unmet `eq` `where` bound. A witness whose `eq` carries an unmet bound is now
refused where the value is ERASED into the protocol slot — a compile-time rejection, not the runtime
defer this section describes — because Chezzi can see the concrete witness at that point, unlike Go's
interface `==` which cannot. See `checker::tests::protocol_erasure_eq_where_bound_boundaries` for the
corrected, narrower half; the rest of this section (a SATISFIED witness deferring to runtime, and
`[T: Eq]` agreeing with `==`) is unchanged.

The original filing framed this as "`Ty::Protocol` is the `Eq` bound guard's blind spot, in two
places" — as if the checker were failing to catch something it should catch. **That framing is
withdrawn for the operator/`.contains()` half.** Protocols in Chezzi are Go interfaces (`CLAUDE.md`:
*"Go for concurrency/interfaces"*), and Go's answer to comparing two interface-typed values is to
compile the comparison unconditionally and panic at the point it actually runs, if the dynamic type
turns out uncomparable. Chezzi's `==`/`.contains()` already gave the same answer at the same point,
for the same reason — verdict, not a fix, for that half.

**But an independent review of the verdict found it under-scoped: a THIRD spelling of the same
relation still disagreed.** `fn generic_eq[T: Eq](a: T, b: T) -> bool: return a == b` fed the exact
`a`/`b` the operator already handled was *type error: type Sized_ does not satisfy Eq* — `==` and a
`[T: Eq]` bound giving two different answers to one relation, the precise invariant `W7-41` exists to
hold, and the same defect SHAPE `W7-54` (`Ty::Func`) had just closed a few hours earlier on this same
branch. **This part IS a bug, per this file's own rule** ("an unsure verdict must decline, never
guess a confident wrong answer" — refusing `[T: Eq]` here was not a decline, it was a wrong answer
the operator itself already disproved). Fixed same day, same session, rather than filed — see below.

**Go, measured, 1.26:**

```go
type Sized interface{ Size() int }
type Tag struct{ ns []int }          // uncomparable: holds a slice
func (t Tag) Size() int { return len(t.ns) }

func main() {
    var x Sized = Tag{[]int{1}}
    var y Sized = Tag{[]int{2}}
    fmt.Println(x == y)
}
```
```
panic: runtime error: comparing uncomparable type main.Tag
```

**Go also re-measured on the generic spelling, 1.26** (Go 1.20+ widened `comparable` to admit
interface types):

```go
func eqg[T comparable](a, b T) bool { return a == b }
var a Sized = Tag{1}; var b Sized = Tag{2}
fmt.Println(eqg(a, b))     // compiles and runs — false, for a COMPARABLE witness
// fed the uncomparable Tag above instead: compiles, panics at the comparison — identical to the operator
```

**Chezzi, measured on the release binary, both engines (`--serial` and default M:N identical):**

```chezzi
protocol Sized_:
    fn size(self) -> int

struct Tag:
    n: int
    fn size(self) -> int:
        return self.n

struct Box[T]:
    val: T
    fn eq(self, o: Box[T]) -> bool where T: Comparable:
        return self.val.compare(o.val) == 0
    fn size(self) -> int:
        return 1

x: Sized_ = Box(Tag(1))
y: Sized_ = Box(Tag(2))
print(x == y)
```
```
runtime error (line 12, col 16): struct 'Tag' has no method 'compare'
  at eq (called at line 18, col 7)
```

The container half is the same shape: `xs: List[Sized_] = [x]` then `xs.contains(y)` faults
identically (`struct 'Tag' has no method 'compare'`, `at eq (called at line 19, col 7)`) — Go's
`slices.Contains` over `[]Sized` panics the same way, for the same reason. **Both programs were
re-run against the pre-change binary and produce byte-identical output on all four combinations**
(HEAD × prehead, M:N × `--serial`) — this half genuinely predates the row and nothing here regressed.

**The `[T: Eq]` fix, same shape as `W7-54`, no VM change.** Root cause: `Ty::Protocol` had no
`Checker::intrinsic_recv_kind` arm (`src/checker/proto.rs:335`, now classifying it `"protocol"`) — the
exact hole `Ty::Func` had — so the D1 `Eq` grant (`src/checker/proto.rs:1780`) excluded it before
`eq_bounds_unsatisfied` was ever consulted, while the OPERATOR never asks the protocol system at all
(it routes through `may_be_equal`) and so kept answering correctly the whole time. Three edits:

1. `Ty::Protocol(..) => "protocol"` added to `intrinsic_recv_kind` (`src/checker/proto.rs:335`).
2. `("Eq", "eq", "protocol")` added to `INTRINSIC_PROTO_METHODS` (`src/checker/proto.rs:189`).
3. A `"protocol"` `Recv` probe row added to `intrinsic_grants_all_have_vm_arms`'s matrix
   (`src/vm/tests.rs`) — the probe template is a bare `a := {lit}` with no annotation, so the row's
   prelude carries a factory `fn mkp() -> Sized_: return Tag(1)` and `lit = "mkp()"`, making `a`
   genuinely infer `Ty::Protocol` rather than the concrete `Tag` — confirmed by running it, not
   assumed.

**Why neither of the row's TWO ORIGINAL arms needed to change, and neither can be narrowed even if
narrowing were wanted** (both re-verified against the widened grant, not merely re-asserted):

1. **`eq_bounds_unsatisfied_rec`'s `Ty::Protocol` catch-all (`src/checker/proto.rs:2852`) staying
   `None` is what makes the new grant SOUND, not just permitted.** `Ty::result(inner)` is literally
   `Ty::Result(inner, Ty::Protocol("Error"))` (`src/checker/ty.rs:304-313`, re-confirmed) — every bare
   `Result[T]` carries an existential in its error slot. Refusing the arm un-grants `Result` wholesale
   and breaks the `("Eq", "eq", "result")` ratchet row. Since this fix, the arm is ALSO what a
   protocol-typed value's OWN `[T: Eq]`/`.eq()`/`==` routes through directly, not merely one nested
   inside a container or `Result`'s error slot — the D1 gate now classifies `Ty::Protocol` as kind
   `"protocol"` instead of turning it away at `"?"`, so this `None` is the grant itself.
2. **`may_be_equal`'s `(Protocol(..), Protocol(..)) => true` arm (`src/checker/proto.rs:1027`) is
   unchanged and untouched by this fix.** It still short-circuits before either operand's witness is
   visible, for the reason recorded in §B2: one concrete type can inhabit two unrelated protocols at
   once, so `Shape == Error` must compile. **Correction to the row's own prior wording:** it is not
   racing a "nominal arm that comes after it" — the arm immediately following is Protocol-vs-CONCRETE
   (asking conformance in the wrong direction for a pair of existentials), not a nominal type arm at
   all, so nothing here was ever "ahead of" anything. That arm's permissiveness is §B2's own open
   question, not this row's.

The deeper reason both arms hold is the same one `W7-41`/`W7-45`/`W7-53` all lean on: the checker
deliberately erases existentials, so it does not know which concrete types inhabit a protocol at a
given call site. A checker that refused every protocol-typed `==`/`[T: Eq]` would reject working
programs (`Box[int]` compares fine — see the positive control below); one that guessed would emit a
confident wrong answer. Declining and deferring to a clean, catchable runtime fault is the correct
verdict for BOTH spellings — the bug was that one of the two spellings was declining too hard (a flat
reject, not a defer), not that either should have guessed.

**Guard rail, swept by `vm::tests::intrinsic_grants_all_have_vm_arms`'s full (protocol ×
receiver-kind) matrix — `Eq` is the ONLY protocol widened by this fix.** `[T: Stringable]` /
`[T: Hashable]` / `[T: Iterable[T]]` / `[T: Add]` at `T = Sized_` all still correctly reject (pinned in
`checker::tests::protocol_typed_value_does_not_satisfy_other_protocols`) — `Eq`-satisfaction is
defined as exactly what `==` already accepts (`W7-41`), and `==` already accepted a protocol-typed
operand; no other protocol shares that premise, so widening one did not widen the rest. `[T:
Comparable]` (which EMBEDS `Eq`) at `T = Sized_` used to report the confusing *"does not satisfy
Eq"* — Sized_ failed the EMBED, not its own `compare`; after the fix the `Eq` embed passes (deferred
to the witness, same as the bare bound) and the rejection names the real gap, *"does not satisfy
Comparable"* (pinned in `checker::tests::protocol_typed_value_rejects_comparable_on_the_real_reason`,
which also asserts the OLD message no longer appears). Protocol-to-protocol assignability
(`[T: Sized_]` fed a `Sized_` value — Go's interface-to-interface assignment) is decided entirely by
`Ty::Protocol`'s own structural arm and was never routed through the D1 kind gate this row widens —
unaffected, pinned in `checker::tests::protocol_typed_value_still_satisfies_its_own_protocol` so a
future change to D1 can't be mistaken for what makes that case work.

**Tests.** Checker-side (`src/checker/tests.rs`): `protocol_typed_value_satisfies_eq` (both bound
spellings, plus the Critical's own `generic_eq` repro verbatim), `protocol_typed_value_does_not_
satisfy_other_protocols`, `protocol_typed_value_rejects_comparable_on_the_real_reason`,
`protocol_typed_value_still_satisfies_its_own_protocol`. Runtime (`tests/chz/spec/
eq_protocol_existential_test.chz`, 10 tests, gated serial==M:N by
`test_runner::chz_suite_passes`): the original 4 — the operator/`.contains()` faults
(now asserting the TIGHTER `"has no method 'compare'"`, not just `"compare"`, which also matched the
checker's own *"cannot compare X and Y"* text) and the positive control, whose `Box[T]` payload now
carries a SECOND field (`tag`) `eq` deliberately ignores, so a silently-stopped dispatch that fell
back to structural `Box` equality (rather than genuinely reaching the witness) would fail it, not
pass it by coincidence — plus 2 new: a `[T: Eq]` bound agreeing with the bare operator on a satisfied
witness (the Critical's fix, both directions — equal and unequal Tags), and the SAME generic bound
faulting cleanly, naming `compare`, on the unsatisfied `Box[Tag]` witness, matching Go's re-measured
`eqg[T comparable]` panic. All pinned tests were run on the actual release binary, both engines, not
assumed from the checker tests alone.

### W7-53 — a generic body that compares must declare the bound — **FIXED 2026-08-12**

`W7-41`/`W7-45` closed the *declared* conditional-`eq` hole (a concrete type whose own `eq` carries an
unsatisfiable `where`); this row is the *erased* one they both filed as a residual: a generic body is
checked ONCE with its type parameter abstract, and the four use-site gates (`==`/`!=`, `in`, the
`List` equality builtins, the map/set key check) erased a free `Ty::Param` to `Ty::Unknown` before
asking `eq_bounds_unsatisfied`, so a `T` that would fail an `Eq` bound was treated as "not yet chosen"
instead of "chosen and failing" — even though the checker's OWN `Ty::Param` arm already refused an
unbounded `T`, it was never reached. Three measured instances, all `ok: no type errors` then
`runtime error (line 7, col 12): struct 'Tag' has no method 'compare'` on **both** engines, fed
`W7-41`'s `Box[T]` (declares `fn eq(self, o: Box[T]) -> bool where T: Comparable`) over `Tag` (no
`compare`):

```chezzi
fn f[T](a: T, b: T) -> bool:
    return a == b
print(f(Box(Tag(1)), Box(Tag(2))))          # instance 1 — the operator

fn f[T](xs: List[T], b: T) -> bool:
    return xs.contains(b)
print(f([Box(Tag(1))], Box(Tag(2))))        # instance 2 — List.contains

fn f[T: Hashable](xs: List[T]) -> int:
    return Set(xs).len()
print(f([Box(Tag(1)), Box(Tag(2))]))        # instance 3 — a Set built inside the body
```

**Ancestor verdicts, measured, not assumed — both owning ancestors reject the DEFINITION, not the call
site:**

| | verdict on `fn f<T>(a: T, b: T) -> bool { a == b }` |
|---|---|
| rustc 1.97.0 | ``error[E0369]: binary operation `==` cannot be applied to type `T` `` + *help: consider restricting type parameter `T` with trait `PartialEq`* |
| Go 1.26 | `invalid operation: a == b (incomparable types in type set)` |
| CPython 3.14 | accepts (dynamically typed — not an authority here) |

Chezzi now matches: all three instances above reject at `f`'s own declaration, naming the fix —
`cannot compare T and T for equality — T is not bounded by Eq (add an `Eq` bound to T: `[T: Eq]`, or
`where T: Eq` on a fn)` for instance 1, `contains() compares List[T] elements for equality — T is not
bounded by Eq (…)` for instance 2, `Set element type T is not bounded by Eq (…)` for instance 3 — the
message names BOTH spellings because this arm is reached from a fn body (`where` is grammar) AND from
a bare type annotation like a struct field's `Map[K, int]` (`where` is not — I2, a same-day
follow-up review finding). Fixed with
`[T: Eq]` (instances 1–2) / `[T: Hashable + Eq]` (instance 3), all three then type-check and run
correctly; `Box[Tag]` itself, fed to a properly-bounded `[T: Eq]` fn, still correctly rejects at the
CALL site (`type Box[Tag] does not satisfy Eq (method 'eq' requires Tag: Comparable)`) — the two
obligations (bound the type parameter; satisfy it at the call site) are independent and both still
enforced.

**The fix: four call sites switched from the erasing variant to the plain one; the erasing variant had
no remaining caller and was deleted.** `eq_bounds_unsatisfied_erased` (formerly `src/checker/proto.rs`)
substituted every free `Ty::Param` to `Ty::Unknown` before calling `eq_bounds_unsatisfied`, so its own
`Ty::Param` arm — which already refuses a param not bounded by `Eq` — never fired for a free `T`.
Switched to the plain, non-erasing `eq_bounds_unsatisfied`: `src/checker/pattern.rs`'s `==`/`!=` gate
(both operands — `may_be_equal`'s int/float and bytes/bytearray cross arms make a left-only gate
asymmetric), `src/checker/pattern.rs`'s `in`-over-a-`List` gate, `src/checker/expr.rs`'s
`contains`/`index_of`/`dedup`/`unique` gate, and `src/checker/proto.rs`'s `Checker::key_ty_reject` (the
map-key/set-element choke point). After the switch `eq_bounds_unsatisfied_erased` had no caller left
(`grep` confirmed) and was deleted along with its doc comment, rather than kept as a dead helper. The
`Ty::Param` arm's message was extended to name the fix, the way rustc's `help:` does — from `{ty} is
not bounded by Eq` to, after the I2 correction below, `{ty} is not bounded by Eq (add an `Eq` bound to
{ty}: `[{ty}: Eq]`, or `where {ty}: Eq` on a fn)` — and reads sensibly at all four wrapping sites
(`"cannot compare … — …"`, `"cannot test membership of … — …"`,
`"…() compares List[…] elements for equality — …"`, `"…key/element type …"`).

**I2 correction, same-day follow-up review: the original message named only the `where` spelling,
which is a PARSE error at a declaration site.** `<whereClause>` exists only on `fn`/`native fn`
(`docs/reference/grammar.bnf:150-165`) — `struct Reg[K: Hashable]: m: Map[K, int]` reaches this SAME `Ty::Param`
arm (via `key_ty_reject` → `sig.rs`'s `Map`/`Set` type-annotation arms), and `struct Reg[K: Hashable]
where K: Eq:` is `parse error (line 1, col 25): expected ':', found 'where'`. The arm is reached from
both a fn body (where `where T: Eq` is grammar) and a bare declaration-site annotation (where it is
not), so the message now names both spellings rather than assuming a fn body.

**`Comparable` already embedded `Eq` (M23), so every `[T: Comparable]` site in the tree survived
untouched — verified**, including every `sort`/`max`/`min`-style stdlib call. `Hashable` did NOT embed
`Eq`, which is what left instance 3 open (`[T: Hashable]` alone was never enough to make an INTERNAL
`Set(xs)`/`{x: 1}` construction sound).

**The `Hashable` route was decided BY MEASUREMENT, not by the initial plan.** Rust's own bound is
`impl<T: Eq + Hash> HashSet<T>` — TWO bounds, not one — and the natural-looking fix (embed `Eq` into
`Hashable`'s `ProtocolInfo`, mirroring `Comparable`'s embed) was tried first and MEASURED to regress a
working, already-relied-upon program: `key_ty_reject`'s first conjunct (`self.satisfies(t,
"Hashable")`) would then transitively demand FULL `Eq` satisfaction (`satisfies_args_d`'s embed-walk),
which for a CONCRETE receiver means an exact structural signature match on any method literally named
`eq` — rejecting the in-tree "ordinary escape hatch" (a struct defining `hash` plus an `eq` that is
NOT the `eq(self, other: Self) -> bool` hook, e.g. `fn eq[U](self, o: U) -> bool: return true`), which
falls back to structural equality at runtime and never faults. Measured on the debug binary with the
embed added:

```chezzi
struct Holder[T]:
    v: T
    fn hash(self) -> int: return 1
    fn eq[U](self, o: U) -> bool: return true
s := Set([Holder(1)])   # rc=0 BEFORE the embed; type error "must implement Hashable…" WITH it
```

This is the exact regression class the `2026-08-10` `satisfies(t, "Eq")` attempt was reverted for
(`W7-41`'s row), re-derived independently for `Hashable`'s embed rather than assumed away. Per the
brief's decided-in-advance contingency, the embed was reverted and the FALLBACK taken instead:
**explicit `[K: Hashable + Eq]` at every affected declaration** — the two-bound spelling Rust's own
`HashSet<T>`/`HashMap<K, V>` already use, and the more faithful mirror of the ancestor (Rust's `Hash`
trait has no `Eq` supertrait either; `Eq + Hash` is spelled at the container, not embedded). `Hashable`
stays `embeds: Vec::new()` in BOTH `std/prelude.chz` and the Rust seed `prebuilt_protocols`
(`src/checker/mod.rs`), so `Checker::assert_native_protocol_shape_matches`'s byte-match assertion needs
no change either.

**Blast radius, measured by building, not by grepping and trusting the grep:** every `[K: Hashable]` /
`[T: Hashable]` in the tree that itself constructs or indexes a `Map`/`Set` keyed on that parameter
needed `+ Eq` added — `native struct Map[K: Hashable, V]` / `native struct Set[T: Hashable]`
(`std/prelude.chz` — their OWN native-fn signatures re-spell `Map[K, V]`/`Set[T]` as param/return
types, which the map/set key gate now judges directly), `std/collections.chz`'s `Counter[T: Hashable]`,
`std/memoize.chz`'s `_memo_get`/`memoize1`, every `[K: Hashable]` in
`std/concurrency/collection.chz` (`ConcurrentMap`, `ConcurrentCounter`, and their six free-fn helpers),
and `examples/newtype_generic.chz`'s `Tally[T: Hashable] = Map[T, int]`. None of these needed anything
beyond the bound spelling — no logic changed, no runtime behavior changed (every touched example's
`.expected` golden output still matches byte-for-byte).

**Correction, same-day follow-up review (C1): "no collateral" was true of the DECLARATION-SITE edits
above, but FALSE of the fallback as a whole when it shipped.** `[K: Hashable + Eq]` only spells the
obligation; it is `satisfies(K, "Eq")` at each CALL site that decides whether a concrete `K` meets it,
and that function had an independent, pre-existing bug (`key_ty_reject`'s doc comment, above) that
wrongly refused a type whose only `eq` is the non-hook ordinary-method escape hatch. So `Counter[Key]`
/ `ConcurrentMap[Key, V]` / `memoize1` over a `struct Key: fn hash(self) -> int: return 1; fn eq[U]
(self, o: U) -> bool: return true` all newly rejected a WORKING program (`==`/`Map`/`Set` on a bare
`Key` all ran fine; only the `[K: Hashable + Eq]`-bounded generics broke) — a real, user-visible
regression this row's original report missed because its own measurements never constructed a
non-hook-`eq` type and pushed it through one of the newly-`+ Eq`-bounded stdlib generics. Fixed the
same day by making `satisfies(_, "Eq")` itself agree with `==` on a non-hook `eq` (`Checker::
eq_sig_is_hook`, `src/checker/proto.rs`/`sig.rs`) — the identical two-spellings-disagree defect shape
Tasks 1/2 closed for `Ty::Func`/`Ty::BuiltinFn`/`Ty::Protocol`, just found one bug later. `Counter`/
`ConcurrentMap`/`memoize1` over `Key` all now run correctly (verified on the release binary, both
engines); see `tests/chz/spec/eq_protocol_test.chz`'s `non_hook_eq_satisfies_eq_too`.

**Correction to the correction (C1′), found by re-reviewing the C1 fix — the first cut closed C1 by
re-opening its mirror.** That cut short-circuited `eq_where_unsatisfied` for a non-hook `eq`, reasoning
"`==` stays structural for such a type, so it cannot fault". True of the **operator**, false of the
**bound**: an erased `[T: Eq]` body's `a.eq(b)` dispatches BY NAME to the escape-hatch method, so its
`where` clause then ran unproven. Measured on the release binary at that cut, both operand spellings
that make an `eq` non-hook (a METHOD type param `[U]`, and the OWNER's type param `T`):

```chezzi
struct Tag:
    n: int
struct Box[T]:
    val: T
    fn eq[U](self, o: U) -> bool where T: Comparable:
        return self.val.compare(self.val) == 0
fn eqm[T: Eq](a: T, b: T) -> bool:
    return a.eq(b)
print(eqm(Box(Tag(1)), Box(Tag(2))))
```
```
chezzi check → ok: no type errors
chezzi run   → runtime error (line 6, col 16): struct 'Tag' has no method 'compare'
                 at eq (called at line 8, col 12) / at eqm (called at line 9, col 7)
chezzi run --serial → identical
```

Verbatim the check-OK-then-fault class this whole W7-5x wave exists to eliminate, on both engines and
therefore parity-blind. **The order is the lesson, and it is the same lesson as W7-41's:** the two
questions a declared `eq` raises are independent and must be asked in the right order — *are its
`where` bounds provable for this instantiation?* (ALWAYS, whatever the shape) and only then *does it
end the walk?* (the HOOK shape only; a non-hook `eq` falls through to the structural member walk,
because structural equality really is what its `==` does). Both `Ty::Struct` and `Ty::Enum` arms of
`eq_bounds_unsatisfied_rec` carry it. Pinned by two `entry_rejects` (one per operand spelling) plus a
premise-derived `ok()` neighbour — the same shapes over a payload that DOES satisfy the bound — in
`checker::tests::struct_with_eq_satisfies_eq`, all three measured against the pre-fix binary first.

**I1′ — `.eq()` through an `[T: Eq]` bound resolved to a non-hook user method where Rust resolves to
the trait — FIXED 2026-08-12.** It was NOT a regression (identical before this wave), and it was a
**silent disagreement, not a fault**, on both engines and therefore parity-blind.

Ancestor verdicts, measured, rustc 1.97.0 — a `#[derive(PartialEq, Eq)] struct Key(i32)` carrying an
INHERENT (non-trait) `fn eq<U>(&self, _o: U) -> bool { true }`:

| spelling | rustc 1.97.0 | Chezzi before | Chezzi after |
|---|---|---|---|
| `a.eq(&b)` — CONCRETE receiver | `true` (inherent wins) | `true` | `true` |
| `a == b` | `false` (`PartialEq`) | `false` | `false` |
| `fn eqm<T: Eq>(a, b) { a.eq(b) }` | **`false`** (the TRAIT method) | **`true`** | **`false`** |

The middle row is the one that used to disagree. `docs/reference/syntax.md` defines `Eq`-satisfaction as
"exactly what `==` already accepts", and `tests/chz/spec/eq_protocol_test.chz` carries a test
literally named `the_method_and_the_operator_agree_on_every_granted_receiver` — so two spellings reaching the same
receiver through the SAME `[T: Eq]` bound answered differently, which is a silent wrong value, the
class this repo ranks below a loud refusal.

**Cause and fix: the compiler is type-blind, so it had to be TOLD.** `a.eq(b)` lowered to a by-name
`Op::CallMethod`, which the VM resolves against the receiver's own method table — it cannot tell a
protocol-dispatched call from an ordinary one. The checker now records, for EVERY one-arg `.eq(x)`
call site, which of the two it is (`Checker::eq_is_protocol_dispatch`, `src/checker/proto.rs`, called
from `infer_method_call`'s single pre-dispatch site in `src/checker/expr.rs`), and the compiler
CONSUMES that verdict: a protocol-dispatched site lowers to **`Op::Eq` — the very opcode `==` emits**
(`Compiler::is_proto_eq_call`, `src/compiler/mod.rs`), so the two spellings are one dispatch *by
construction* rather than by two rules that must be kept in agreement. `Op::Eq` runs
`Vm::values_equal_guarded`, which already dispatches a real user `eq` HOOK and falls back to the
structural walk otherwise — so **a genuine hook is still dispatched through the bound**, which is the
entire point of `Eq` being user-overloadable, and no third notion of "the hook" was introduced.

**No new notion of the hook, and the over-fire was closed in the granting direction.** The predicate is
"the receiver is a `Ty::Param` whose bound RESOLVES an `eq`" — by resolution, not by name, so an
EMBEDDING bound (`[T: Comparable]`, which embeds `Eq`) dispatches the protocol too. But the resolved
protocol method must carry the `Eq` hook signature `fn eq(self, other: Self) -> bool`
(`Checker::is_eq_hook_protocol_sig`), since lowering an unrelated `fn eq(self, o: int) -> bool` to
`==` would compare the receiver against the argument instead of calling the method — an over-fire in
the granting direction, the one that yields a silent wrong value.

> **Scope correction (review).** That gate is **defence in depth, not a closed live bug** — the
> over-fire is unreachable today, and the first wording here implied otherwise. A protocol declaring
> `fn eq(self, o: int) -> bool` has no possible struct/enum witness, because `validate_eq_shape`
> rejects the witness at its own declaration: *"'eq' on S is the `Eq` protocol hook `==` dispatches
> to: its operand must be S, found int — rename the method if it is not equality"* (measured). So
> `[T: Scorer]` over such a protocol is unsatisfiable, and the `Eq`-embed variant is separately caught
> by *"method 'eq' conflicts with embedded protocol requirement"*. The gate keeps the path closed if
> `validate_eq_shape` ever loosens; it was not rescuing a reachable wrong answer.

`Ty::Protocol` receivers need no arm: `eq(self, o: Self)` puts `Self` in a parameter slot, which
`infer_method_call`'s object-safety gate already rejects — verified still true after Task 2 widened
what a protocol-typed value satisfies — with a message pointing at the fix: *"method 'eq' is not
callable through the protocol value Eq — its signature takes `Self` … Bind the receiver with a
generic parameter instead: `[T: Eq]`"*.

**Where a key CONFLICT surfaces, and why that reads oddly.** `reject_table_conflicts` raises a
`CompileError`, and the CLI labels every compile error `runtime error:` — so an aliased key is
`chezzi check` → *ok: no type errors*, then `chezzi run` → *internal: two different '.eq()' dispatch
decisions …*. That framing is pre-existing (it is how the CLI labels the whole `CompileError` class,
which W7-49's own table already joined), not something this change introduced — but this table now
rides it too. It is a build stop with a named workaround, never a wrong value.

**The side table is keyed on the method-NAME token, NOT the call node's span**, reusing
`crate::checker::carrier_key` verbatim — the same derivation `?.` carriers and M24's member witnesses
already use, so there is ONE key derivation and the record site and the lookup site cannot drift. The
node span is shared by every link of a postfix/pipe chain; the `eq` token is a distinct source node
per link, and `Span` has carried file identity since **W7-49**, which is what makes it injective
ACROSS modules. **BOTH verdicts are recorded, never just the `true` one** — that is what turns the
one surviving residual (the same default expression spliced twice into the SAME module) into a hard
`CompileError` via `record_call_table_entry` instead of silently applying one site's dispatch to
another. And a lookup MISS means "ordinary by-name call", which is also the pre-fix lowering, so a
missing entry can only ever under-apply the fix, never mis-apply it. Pointer identity of the `Expr`
node was evaluated as the alternative and REJECTED by measurement, not by taste: the checker
*clone-and-lowers* subtrees (`infer_opt_chain`, `src/checker/pattern.rs`) and infers the CLONE, so a
node address recorded during checking is not always the node the compiler compiles — its failure mode
is a silent miss, and a stale address can collide, where the span key's failure mode is a build stop.

**Evidence.** `tests/chz/spec/eq_protocol_bound_dispatch_test.chz`, 8 `test fn`s, measured against the
PRE-fix release binary first — **5 passed, 3 failed** there (`through_an_eq_bound_the_protocol_wins`,
`the_bound_and_the_operator_agree_on_a_non_hook_receiver`,
`an_embedded_eq_bound_dispatches_the_protocol_too`), all 8 pass after, identical on both engines.
Every struct in it defines an `eq` that DISAGREES with structural equality in at least one direction
(`Key`'s non-hook `eq` returns `true` for everything; `Parity`'s real hook compares `n % 2`, which is
wrong in both directions), so no assertion can pass unless dispatch went the right way. The
concrete-receiver row is pinned in its own test so the inherent-wins rule cannot be "fixed" by
accident. Edge spellings verified on the rebuilt binary, both engines: an interpolation fragment
holding two distinct `.eq` sites (`"{a.eq(b)}|{a.eq(a)}"` → `false|true`), an indexed receiver
(`xs[0].eq(b)`), and a nested `fn` capturing the bounded locals. `W7-53`'s own C1′ repro still rejects
at the CALL site (`type Box[Tag] does not satisfy Eq (Box[Tag]'s `eq` requires Tag: Comparable)`) —
the where-bound obligation is unaffected, because it is proven at satisfaction time, not at lowering.

Two `#[cfg(test)]` fixtures
(`tests/chz/spec/eq_protocol_test.chz`'s `same`/`differs`, used to warm-then-deopt an int-specialized
`==` site onto structs) needed the same `[T: Eq]` addition, as did roughly a dozen `checker::tests`
fixtures across `comparable_types_equality_still_ok`, `contains_generic_struct_substitutes_item`,
`eq_operator_enforces_the_receivers_eq_where_bounds`, `list_equality_builtins_enforce_the_element_eq_
where_bounds`, `a_map_key_or_set_element_must_be_eq_not_merely_hashable`, and
`generic_newtype_ctor_infer_set_underlying_ok` — several of which had DOCUMENTED, in-tree comments
explicitly describing the ceiling this row closes (*"ERASED: … Bare `T` is rejected as before … the
un-erased spelling would break …"*) that were rewritten rather than left stale.

**The `_ => "?"` variant enumeration required by the task brief** (Tasks 1/2 each missed one `Ty`
variant with no `intrinsic_recv_kind` arm — `Ty::Func`, then `Ty::BuiltinFn`, then `Ty::Protocol` —
each found only by review): the remaining `"?"` variants at `Checker::intrinsic_recv_kind` are
`Ty::Param`, `Ty::Module`, `Ty::Unknown`, and the eleven handles `Channel`/`Shared`/`RwShared`/
`Atomic`/`AtomicInt`/`Executor`/`Socket`/`Listener`/`Writer`/`Reader`/`Ptr`.

* **`Ty::Param` must stay excluded — confirmed, not assumed.** This is precisely what this row's fix
  depends on: `eq_bounds_unsatisfied_rec`'s `Ty::Param` arm is the refusal that makes W7-53 work, and
  the D1 `Eq` grant admitting a bare `Ty::Param` (kind `"?"`) would make every unbounded `T`
  intrinsically `Eq`, silently reopening this exact row.
* **`Ty::Module`, `Ty::Unknown`** stay excluded for the reasons already documented at the classifier:
  a module is never a value (`m == m2` is not a spellable program to begin with — measured, `import
  std.time as a; import std.time as b; print(a == b)` is a checker error — **correction, same-day
  follow-up review (M7): the actual message is `cannot compare module a and module b for equality`**,
  not the "module ... is not a valid expression" text originally quoted here, which this binary does
  not produce — before `Eq` is ever consulted); `Ty::Unknown` is the don't-cascade hole (a prior error
  already reported), and admitting it as `"unknown"` would need a construction discipline no other
  kind has (there is no VALUE of type `Unknown` to probe with).
* **The eleven handles compare by identity, and `==` already accepts that — measured** (e.g.
  `import std.concurrency\nc1 := Channel[int](1)\nc2 := Channel[int](1)\nprint(c1 == c1)\nprint(c1 ==
  c2)` prints `true` then `false`, both engines) — but they stay EXCLUDED from the `Eq` GRANT
  (`[T: Eq]`/`.eq()`), and this is the documented, deliberate reason already recorded at the D1 gate
  (`src/checker/proto.rs`, `satisfies_args_d`'s `Eq` arm): admitting them would be a grant with no
  proof obligation and no runtime arm to pair it with (`INTRINSIC_UNPAIRED`'s exact carve-out
  condition) for zero behavioral gain — there is no declared-`eq`/structural-walk question to ask of an
  identity-only receiver, unlike `Ty::Func`/`Ty::BuiltinFn`/`Ty::Protocol` (Tasks 1/2), which had a real
  proof obligation (`eq_bounds_unsatisfied` needed to route through them) that made the grant worth
  something. **Correction, same-day follow-up review (M6): "no constructible probe receiver … need real
  OS resources" was measurably false for six of the eleven.** `Executor()`, `Channel[int](1)`,
  `Shared(1)`, `RwShared(1)`, `Atomic(1)`, `AtomicInt(1)` all construct in a plain program with no OS
  resource beyond the process itself, and `==` runs on every one (measured, both engines) — the
  original wording conflated "no cheap-to-construct value exists" (true only of `Socket`/`Listener`/
  `Writer`/`Reader`, which are RETURNED by a real `connect`/`bind`/`open`, never user-constructed
  directly, and `Ptr`, an FFI value) with "a grant here adds nothing `==` doesn't already give" (true
  of all eleven, and the actual reason to exclude them — restated above). Not fixed, and not filed as
  a new residual — this row IS the record of the decision so the next person does not have to
  re-derive it.

**Tests.** `src/checker/tests.rs`'s `a_generic_body_that_compares_must_bound_its_type_param_by_eq`: the
three measured instances as `rejects()`, each asserting the (I2-corrected) `add an `Eq` bound to T`
help text; `ok()` neighbours derived from the same premise — `[T: Eq]`; `[T: Comparable]` (works via
the existing embed, untouched); a generic fn that takes `T` but never compares it; a partially-generic
`Map[T, Box[Tag]]` where `T` defers to its own declared bound and only the concrete `Box[Tag]` half is
walked structurally; and a generic fn whose `T` is reached only through a declared conditional `eq`
whose `where` bound (`Comparable`) is satisfied by `T`'s own declared bound. `struct_with_eq_
satisfies_eq`'s `Opt2[T]` case FLIPPED from `entry_rejects` to `entry_ok` (C1: a non-hook `eq` must
satisfy `Eq`); `eq_wrong_return_type_rejected`'s arity case now asserts the decl-time `validate_eq_
shape` text directly (C1: D1 no longer runs the now-redundant structural fallback for a NON-2-param
`eq`, so there is one clean error instead of two). Runtime, `tests/chz/spec/eq_protocol_test.chz`:
**`one_eq_bound_covers_every_join_point`** (M9, added — the brief's ask was previously only satisfied
piecemeal across separate files) runs ONE erased `[T: Eq]` bound (`eq_bound`) over `int`, `List`,
`tuple`, `Option`, a struct with a HOOK `eq` (`Ver`, proving the hook actually dispatches, not a
structural fallback), and a **function value** (`Task 1`'s `Ty::Func` grant); **`non_hook_eq_
satisfies_eq_too`** (C1, added) is the regression test for the fix above — a `Key` with only a
non-hook `eq` satisfies `[T: Eq]`/`.eq()` while `k1 == k2` stays structurally `false`, and `Key` works
as both a `Map` key and a `Set` element. All pass identically on the M:N engine and `--serial`.

**Gate:** `cargo build --release` clean; `cargo clippy --all-targets -- -D warnings` clean; full `cargo
test` (lib unit + two-engine parity + conformance + integration) green; `./target/release/chezzi test
tests/chz/` and `--serial` both green, byte-identical verdicts; every touched `examples/*.chz` (
`contains_protocol`, `newtype_generic`, `memoize`, `collections`, `concurrent_collection`,
`concurrent_jobs`) runs and matches its `.expected` golden byte-for-byte.

### W7-55 — the `Eq` walk's depth cap refused a type graph the VM compares happily — **FIXED 2026-08-12**

Task 4 of the four-task `Eq` residual close-out on `w7-52-55-eq-residuals`. The original filing:
`Checker::eq_bounds_unsatisfied`'s in-progress stack was capped at `EQ_BOUNDS_MAX_IN_PROGRESS = 160`
(`src/checker/proto.rs`), sized on the SMALLEST stack the checker ran on — which was the Rust
test-harness thread, an ambient default, not production (`crate::on_frontend_stack`'s 1 GiB wrapper
already covered the CLI and `editor::diagnostics`/`editor::hover`, but not the checker's own
`#[cfg(test)]` entry point or its production `check_graph_with_entry`). A chain `struct S0: v: int` …
`struct S{N}: v: S{N-1}` plus builders, `mk{N}() == mk{N}()`, RAN and printed `true` for N ≤ 159 and
was *S160 nests too deeply …* from N = 160 — a checker-only rejection of a program the VM's own
`values_equal_guarded` (no cap at all) ran fine, at a depth two orders of magnitude smaller than
anything a hand-written type graph would plausibly reach.

**The obvious fix — raise the floor, then raise the cap — is only half right, and the other half OOM-
killed the machine on the first attempt.** The floor half is exactly what the brief asked for and
landed clean (below). The cap half assumed the walk's cost was uniform per level, sized only by STACK.
It is not, for the one shape the cap exists to terminate.

**The O(cap²)-MEMORY finding — the reason "just raise it" was wrong.** `EQ_BOUNDS_IN_PROGRESS` holds a
CLONE of every in-progress `Ty`. For a NON-growing shape (`S0..S{N}`) every level's `Ty` is the same
small size, so N levels cost O(N) memory. For a GROWING shape — polymorphic recursion, `struct N[T]: v:
T; next: Option[N[List[T]]]` — level *k*'s `Ty` has size O(k) (`N[int]`, `N[List[int]]`,
`N[List[List[int]]]`, …), so the walk is O(cap²) in memory, not O(cap). Measured on the release binary,
`mk(1) == mk(1)` over the polyrec shape, cap raised with no other change:

| cap | wall | peak RSS |
|---|---|---|
| 160 | 14 ms | small |
| 2 000 | **1 342 ms** | **621.8 MB** |

Clean quadratic — 12.5× the cap, ~96× the time, ~(12.5)² the memory. The identical cap on the
NON-growing control (`S0..S159`) was 17 ms / 13.6 MB at cap 2 000 — the cost is entirely the growth,
not the depth. Extrapolating the naive fix to a stack-safety-sized cap (tens of thousands, see below)
projected to tens of minutes and hundreds of gigabytes; the first attempt at exactly that OOM-killed
the development machine and ended its terminal session.

**The corrected design: two guards, not one number doing both jobs.**

**Guard A (new) — growth detection, `Checker::is_growing_over`.** When the walk is about to re-enter a
nominal name that is already in progress under a DIFFERENT instantiation, ask whether the new
instantiation strictly CONTAINS the in-progress one at every type-argument position (with at least one
position strictly larger) — `Checker::ty_contains_or_eq` walks `List`/`Set`/`Option`/`Map`/`Result`/
`Tuple`/`Struct`/`Enum`/`NewType` structurally. If so, that is polymorphic recursion and the walk
refuses immediately, via the existing `eq_budget_refusal`/`EQ_BUDGET_MARKER` machinery (no new message
needed — "nests too deeply" is equally true of "refused for growing" and "refused for exceeding the
cap"). Tracked by a new thread-local, `EQ_BOUNDS_IN_PROGRESS_BY_NAME` (a `HashMap<String, Vec<Ty>>`
bucketing in-progress instantiations by nominal name), kept in lockstep with the existing
`EQ_BOUNDS_IN_PROGRESS` `Vec` at its same two mutation sites; per name the entries nest LIFO with the
rest of the call stack, so `EqObligation::drop` can `pop()` the bucket rather than search it.

**Rust agrees the growing shape is unprovable** — measured, rustc 1.97.0 (`scratchpad/polyrec.rs`):
`#[derive(PartialEq, Eq)] struct N<T: Eq> { v: T, next: Option<Box<N<Vec<T>>>> }` is `error[E0320]:
overflow while adding drop-check rules for N<i32>`. Guard A is the mechanism that reaches that same
answer cheaply — at depth ~2, independent of the cap — rather than the mechanism that reaches it at
all; the cap already refused it before this row, just expensively.

**The relation is DIRECTIONAL, and this repo has shipped the symmetric mistake three times before**
(`docs/contributing/gaps.md` W7-42's lesson, restated in the addendum that redirected this task after the OOM: *"a
two-operand guard whose real question has a direction must be asymmetric, or it silently rejects the
ordered half it was never tested against"*). `N[int]` in progress, `N[List[int]]` about to enter —
growing, refuse. `Pair[Pair[int]]` in progress, `Pair[int]` about to enter (an ORDINARY generic struct
instantiated with itself as its own type argument — no special recursive field needed, just
`Pair[Pair[int]]` written by the user, whose field `v: T` walks to `Pair[int]` one level down) — that
is a SUBTERM of what's already in progress, shrinking, terminates on its own, and MUST stay accepted.
Built as a test — `a_self_nested_generic_instantiation_shrinking_inward_is_accepted`, including a
TWO-level-shrink neighbour a symmetric "the args differ" guard would reject on the FIRST comparison —
**before** the guard, per the addendum's instruction, and it passes.

**Guard B — the depth cap, now doing only its stack-safety job.** With Guard A removing the growing
shape from this cap's reach entirely, every walk the cap now bounds is non-growing (O(1) `Ty` size per
level), so the cap is O(cap) total, not O(cap²) — the brief's original re-derivation procedure applies
unmodified. **Floor re-measured** (cap lifted for probing, DEBUG build, on the 1 GiB
[`crate::on_frontend_stack_scoped`] thread — see `checker::tests::stack_probe_eq_bounds_depth`, the
`#[ignore]`d harness this was measured with, invoked directly per-depth under a `systemd-run
--scope -p MemoryMax=4G -p MemorySwapMax=0` cap, one probe process at a time): a chain of CONDITIONAL
`eq` types (the expensive shape, ~5-10 Rust frames per level) survives **118 000** and overflows
(`fatal runtime error: stack overflow`, exit 134) by **119 000** — up from 240/280 on the old ambient
test-harness stack, roughly the ~490× the 1 GiB wrapper implies over whatever that stack actually was.
A chain of plain structs (the cheap shape) survives **570 000** and overflows by **590 000** — up from
800/1600. `EQ_BOUNDS_MAX_IN_PROGRESS` moved from 160 to **78 000** — the expensive floor (118 000) over
1.5, matching exactly how 160 was derived from 240, rounded down for extra headroom (78 000 × 1.5 =
117 000 < 118 000).

**Cost at the new cap, measured on the release binary — both fixtures the addendum required:**

| fixture | wall | peak RSS |
|---|---|---|
| polyrec (`mk(1) == mk(1)`, refused) | **3 ms** | **13.6 MB** |
| `S0..S159` plain chain (`mk159() == mk159()`, accepted, runs, prints `true`) | 19 ms | 13.6 MB |
| `S0..S70000` plain chain, `chezzi check` only (near the new cap) | 1.225 s | 361.9 MB |

The polyrec fixture is UNCHANGED from the cap-160 baseline (14 ms) despite the cap being 487× larger —
Guard A, not the cap, decides it now, and does so before the `Ty` has grown past two levels. The
near-cap plain-chain probe (70 000 of 78 000) confirms the SEEN-index fix (below) keeps the walk linear
at the new size rather than reverting to O(cap²): scaling from the 159-link baseline (19 ms) by
70 000/159 ≈ 440× predicts ~8.4 s under a naive O(N²) model; the measured 1.225 s is close to the O(N)
prediction (440× the per-level constant) instead.

**A second, independent O(cap²) fix, needed once the cap could get this large regardless of Guard A:**
`Checker::enter_eq_obligation`'s exact-match cycle check was a linear `Vec::contains` SCAN over
`EQ_BOUNDS_IN_PROGRESS` — cheap at cap 160, but O(cap) per call × O(cap) calls = O(cap²) TIME at
cap 78 000 even for ordinary non-growing chains. Fixed with `EQ_BOUNDS_IN_PROGRESS_SEEN`, an O(1)
`HashSet<Ty>` membership index kept in lockstep with the existing ordered `Vec` (which stays the source
of truth — `eq_budget_refusal` reads its first entry to name the walk's root). `Ty` gained `derive(Eq,
Hash)` for this (`src/checker/ty.rs`) — sound: every variant already had a total, structural
`PartialEq`, and the one hand-written `PartialEq` (`FnLabels`, deliberately equality-neutral — every
value equal) got a matching hand-written `Eq`/`Hash` (hash nothing, to keep "equal values hash equal").

**The frontend-stack floor fix, unchanged from the brief.** `src/lib.rs` gained
`on_frontend_stack_scoped`, a borrow-friendly sibling of `on_frontend_stack` built on
`std::thread::scope` + `Builder::spawn_scoped` (same `resume_unwind` panic propagation, but takes
`&Module`/`&ModuleGraph` without a `'static` clone). `checker::check` (`#[cfg(test)]`-only — the
`check_src`/`ok`/`rejects` entry point) and `checker::check_graph_with_entry` (the production path
`check_graph` delegates to — CLI, LSP diagnostics/hover, `test_runner`) both wrap their body in it, so
every one of the ~4000 Rust test-harness callers is now on the SAME 1 GiB stack as production, by
construction, rather than by a convention each future caller has to remember. Per-call cost of the
extra thread spawn is negligible — measured, 4 000 scoped 1 GiB-stack spawns (a synthetic microbench
matching the test-suite's actual call volume): **194.5 ms total, 48.6 µs average**, against a
`cargo test --lib checker::` suite that runs in ~14 s.

**`docs/contributing/gaps.md` W7-50's `editor::semantic_tokens` row does NOT get the same coverage, checked rather
than assumed.** `editor::semantic_overlay` (which `semantic_tokens` calls) invokes `lexer::tokenize` +
`parser::parse` directly — it never calls `checker::check` or `checker::check_graph_with_entry`, so
neither wrapped entry point covers it. W7-50 was left exactly as filed then; **it is FIXED as of
2026-08-13** — `semantic_tokens` now hops onto `crate::on_frontend_stack` itself, and both parser depth
constants were re-derived from a measurement. See the W7-50 row.

**The boundary pair at the new cap, both directions proven, resource-appropriately.** `S0..S77999`
(78 000 simultaneous obligations at peak) is the last depth the cap accepts; `S0..S78000` (78 001) is
the first it refuses — both sides pinned CHECK-only
(`checker::tests::the_new_cap_boundary_accepts_then_refuses`, 466 MB / 5.3 s), after generating both
~78 000-struct sources and compiling+running one of them in the same test process first pushed a 6 GiB
cap into an OOM-kill (a real cost of a genuinely-huge program, not a defect in the guard). A follow-up
by-hand attempt to actually RUN `S0..S77999` via the release CLI, alone, nothing else competing for
memory, OOM'd even at a 12 GiB cap — a 78 000-deep chain of nested FUNCTION CALLS costs real VM stack/
frame memory independent of anything this row touches, and chasing it is out of this task's scope.
`checker::tests::runtime_boundary_is_honoured_at_a_depth_the_old_cap_refused` instead proves the grant
is honoured AT RUNTIME (not just claimed by the checker) at an affordable depth (2 000) that is still
far past the OLD 160/240 caps — the shape that used to be wrongly refused.

**What still gets refused, and in bounded time — the whole point of Guard A.**
`conditional_eq_bound_recursion_terminates`'s polymorphic-recursion case still rejects with `nests too
deeply`; `polymorphic_recursion_is_refused_in_bounded_time_by_growth_detection` (new) times it —
must stay under 500 ms regardless of how high the cap is set, which would not hold if growth detection
were absent (the O(cap²)-memory table above is what the walk would cost the OLD way at this cap).

**Tests.** New: `a_self_nested_generic_instantiation_shrinking_inward_is_accepted` (Guard A's
DIRECTIONAL shrinking half, single- and two-level), `polymorphic_recursion_is_refused_in_bounded_time_
by_growth_detection`, `the_new_cap_boundary_accepts_then_refuses`, `runtime_boundary_is_honoured_at_a_
depth_the_old_cap_refused`, `stack_probe_eq_bounds_depth` (`#[ignore]`d, the by-hand floor-measurement
harness, kept for the next time this floor needs re-deriving). Updated for the new cap:
`a_long_chain_of_conditional_eq_types_is_decided_not_waved_through` (its past-the-cap case moved from
180 to 79 000), `the_budget_refusal_names_the_type_the_walk_started_from` (`S200` → `S78500`, so it
still trips the budget at all). `a_deep_but_finite_chain_stays_within_the_eq_budget` (140-link chain)
needed no change — 140 was never close to either the old or new cap.

**Gate:** `cargo build --release` clean; `cargo clippy --all-targets -- -D warnings` clean; `cargo test`
**4288 passed / 0 failed** (was 4284; +4 new Rust test fns — the `#[ignore]`d probe doesn't count);
`./target/release/chezzi test tests/chz/` and `--serial` both **493/493 identical** (unchanged — no
`.chz` test added or removed, this row is Rust-only). Full commands and raw output in the W7-55 session
report (`.superpowers/sdd/w752-task-4-report.md`).

---

#### Same-day corrections (2026-08-13) — an independent adversarial review found two CRITICALs in the
write-up above, and closing them changed the shipped cap. Read this before trusting `78 000`,
`ty_contains_or_eq`, or "refuses on the first growing step" anywhere above as still current.

**CRITICAL — Guard A was blind to growth through `Ty::Func` (a regression AND a live OOM).**
`ty_contains_or_eq` (quoted above as walking `List`/`Set`/`Option`/`Map`/`Result`/`Tuple`/`Struct`/
`Enum`/`NewType`) fell to a `_ => false` default for everything else — a **fail-open default inside a
termination guard**, the same class of mistake this branch closed twice already (T2/T3's hook-`eq`
escape hatch) just in the granting direction instead of the checking direction. `Ty::Func` is a legal
`Eq` type argument (W7-54, same branch) with `params`/`ret` children that can themselves grow, and it
fell straight through that default. Repro:

```chezzi
struct N[T]:
    v: T
    next: Option[N[fn(T) -> int]]
fn same[T: Eq](a: T, b: T) -> bool:
    return a == b
fn use_it(a: N[int], b: N[int]) -> bool:
    return same(a, b)
```

Measured, `chezzi check` under `MemoryMax=2G timeout 30`: pre-this-correction (`43e28cf3`), **exit
124 at 30 s**, observed by the reviewer reaching >4 GiB before an OOM-kill at 97 s — the exact O(cap²)
blow-up this row's own write-up measured and thought it had closed, reopened through the one
containment arm that wasn't exhaustive. Post-correction: refuses cleanly, **5 ms / 13.4 MB** (matching
the pre-W7-55 `b0cb15b8` baseline's shape of answer).

**Fix: `ty_contains_or_eq` is deleted, not patched.** Replaced with `Checker::ty_nodes`, a total
node-count size metric with **no `_` catch-all arm** — every `Ty` variant is matched explicitly, so
adding one is a compile error until someone decides how it counts. `is_growing_over` now compares
`ty_nodes` per argument dimension instead of structural containment. The lesson, stated plainly because
it is now the second time this exact shape has bitten this branch: **a termination/soundness guard
must never have a silent default in the granting direction** — `_ => false` here read as "assume this
variant can't be part of growth," which is precisely the assumption a guard exists to never make
unchecked.

**CRITICAL — Guard A refused a terminating type graph rustc accepts, and the verdict depended on an
unrelated root instantiation.** Containment is not evidence of non-termination: a nominal naming a
**constant** larger instantiation of itself is finite and closes on the exact-match guard one level
down — Guard A's original cut (refuse on the FIRST growing re-entry against ANY in-progress ancestor)
could not tell that from real polymorphic recursion. Repro:

```chezzi
struct Wrapper[T]:
    v: T
    ints: Option[Wrapper[List[int]]]     # CONSTANT arg, not List[T]
fn same[U: Eq](a: U, b: U) -> bool:
    return a == b
fn use_it(a: Wrapper[int], b: Wrapper[int]) -> bool:
    return same(a, b)
```

`Wrapper`'s field ignores its own type parameter — every instantiation nests `Wrapper[List[int]]`, a
CONSTANT — so `Wrapper[int]` → `Wrapper[List[int]]` → `Wrapper[List[int]]` (now an EXACT match, closed
by the ordinary cycle guard) is finite at depth 2, and rustc compiles the mirror. Pre-correction:
`type error: … Wrapper[int] nests too deeply …` at depth 2, well inside the OLD 240 cap the original
1600-line write-up above measured this shape could never have hit — Guard A was the only new refusal
path. Sharpest proof the guard was asking the wrong question: identical declaration, verdict flips on
the unrelated ROOT instantiation — `use_it(a: Wrapper[int], …)` refused (`int ⊂ List[int]`),
`use_it(a: Wrapper[str], …)` accepted (`str ⊄ List[int]`) — precedent inside this very file (W7-42) for
what a wrongly-cut directional guard does.

**Fix: refuse only on the SECOND CONSECUTIVE growing re-entry of the same nominal**, checked against
the nearest in-progress ancestor only (not every ancestor in the bucket). A constant-argument field
reaches a fixed point after exactly one substitution — its next re-entry is an exact match, caught by
the cycle guard before Guard A runs again — so it can never accumulate two growing steps in a row,
while true polymorphic recursion grows at *every* level and still trips this looser rule in bounded
work (~3 levels instead of ~2). `EQ_BOUNDS_IN_PROGRESS_BY_NAME` now carries a `(Ty, streak)` pair per
entry instead of a bare `Ty` to track this. Both `Wrapper[int]` and `Wrapper[str]` now check `ok`
(0.004 s / 13.4 MB each); the original polyrec fixture (`N[T]` / `Option[N[List[T]]]`) still refuses,
now at depth ~3 instead of ~2, unchanged in cost (0.004 s / 13.4 MB).

**IMPORTANT — the cap moved again: `EQ_BOUNDS_MAX_IN_PROGRESS` now equals
`crate::vm::MAX_STRUCTURAL_DEPTH` (10 000) by construction, not 78 000.** W7-55's whole complaint was
the checker being STRICTER than the runtime; `78 000` fixed the wrong side of that by simply exceeding
10 000 with an arbitrary bigger number, leaving a `checker-superset-of-compiler` window open: measured,
a declare-only `S0..S40000` chain type-checks `ok` in ~0.5 s / 196 MB, while `chezzi run` on the SAME
program is OOM-killed at a 4 GiB cap — the checker granting an `Eq` compare the backend cannot itself
lower. `MAX_STRUCTURAL_DEPTH` is now `pub(crate)` and `EQ_BOUNDS_MAX_IN_PROGRESS` references it
directly (`src/checker/proto.rs`), so the two literally cannot drift apart again. The new cap still
sits comfortably under every measured stack-safety floor (118 000 for the expensive `cond` shape — see
the M7 note below on why the `nonhook` shape wasn't independently re-measured), so Guard B is not the
tight constraint; runtime agreement is.

**IMPORTANT — the 1 GiB floor was not actually structural: two checker passes on the COMPILE path
still ran on the VM's 384 MiB thread, unwrapped.** `checker::resolve_extern_signatures` and
`checker::resolve_call_tables` (`src/checker/mod.rs`) run the same `run_graph_pass` walk as the two
wrapped entry points, but `compiler::compile_graph` calls them directly, and `compile_graph` itself
runs from inside `vm::run_file_engine`'s `VM_STACK_BYTES` (384 MiB) thread for `chezzi run`/`chezzi
test` — a SMALLER stack than the checker's other two entry points get. The doc block's claim that
"there is no smaller-stack caller left" was therefore false (it named the fix for the exact same class
of hole `editor::hover` used to be, then missed a second instance of it). Both resolver entry points
now wrap their body in `crate::on_frontend_stack_scoped` too, so the compile path is on 1 GiB by
construction like every other caller, and the false doc sentence is deleted.

**MINOR follow-ups applied the same day:**
- The cap-boundary Rust tests (`a_long_chain_of_conditional_eq_types_is_decided_not_waved_through`,
  `the_budget_refusal_names_the_type_the_walk_started_from`, `the_new_cap_boundary_accepts_then_
  refuses`) moved their past-cap depths from ~78 000/79 000 to ~10 000/10 500, matching the new cap.
  Peak RSS at the new depths dropped from ~466 MB/~1 GB/~1 GB to **149.6 MB / 73.6 MB / 73.2 MB**, and
  wall time from ~4.8 s each to **0.63 s / 0.43 s / 0.64 s** — the concurrent-cargo-memory tripwire the
  review flagged is gone at the new cap.
- `the_budget_refusal_elides_a_runaway_type_name`'s first half is now near-vacuous (Guard A refuses
  organic growth before a type name can become huge through recursion alone) and is kept with an
  inline explanation rather than silently dropped; its second half (a manually-huge ROOT, not one grown
  by recursion) is unaffected and still exercises the elision path for real.
- The floor was only ever measured on the `cond` (hook-`eq`) shape; a `nonhook` shape (no custom `eq`
  at all, so every level proves `Eq` through Task 3's unconditional `any(args)` + `walk_eq_members`
  path) was added to `stack_probe_eq_bounds_depth` for future re-measurement, but not run — the new cap
  (10 000) sits at ~1/12 of the `cond` floor (118 000), so `nonhook` would need to cost >12× more per
  Rust stack frame to threaten it, which is not plausible for "one more pass through a few
  combinators."

**Runtime verification, resource-capped:** the Func-growth repro and both `Wrapper` roots and the
polyrec fixture above all check in single-digit milliseconds at ~13 MB. A `S0..S9999` chain (10 000
simultaneous obligations, the exact new-cap boundary) checks `ok` in 0.28 s / 94 MB. Actually RUNNING a
chain at that same boundary hits the SAME pre-existing, unrelated VM cost this row's original write-up
already documented at the OLD 78 000 boundary (nested-nested-function-call construction, not the
`Eq`-walk): measured `S0..S{n}` via `mk{n}() == mk{n}()`, n=3 000 → 795 MB/1.95 s, n=4 000 → 1.40
GB/3.60 s, n=5 000 → 2.16 GB/5.04 s (clearly superlinear, independent of any change in this
correction), and n=9 999 OOM'd at an 8 GiB cap. This is the identical, already-scoped-out cost — just
newly reachable-in-principle because the cap itself shrank from 78 000 to 10 000 — and remains out of
this row's scope; `runtime_boundary_is_honoured_at_a_depth_the_old_cap_refused` (N=2 000, unchanged)
still proves the grant is honoured at runtime, well past the OLD 160/240 caps.

**Gate after this correction:** `cargo build --release` clean; `cargo clippy --all-targets -- -D
warnings` clean; `cargo test` **4290 passed / 0 failed** (was 4288; +2 new Rust test fns —
`a_growing_re_entry_through_a_constant_type_argument_is_accepted`,
`polymorphic_recursion_through_a_func_type_argument_is_refused_in_bounded_time`); `chezzi test
tests/chz/` **493/493 identical** on M:N and `--serial` (unchanged — Rust-only correction). Full
commands and raw output in `.superpowers/sdd/w752-task-4-report.md`.

#### Third correction (2026-08-13, adversarial review) — the classifier was DELETED, not patched again

Three independent prosecutors attacked the branch. Two of them, independently, defeated Guard A **in
both directions again** — the same two directions it had already been repaired in once. Every charge
below was reproduced on the release binary before being acted on.

**Charge A (critical) — argument permutation evaded the guard, into an OOM.** Guard A fired only when
*every* type argument was non-decreasing, so a nominal that permutes its parameters while growing one
never accumulated a streak:

```chezzi
struct M[A, B]:
    v: A
    w: B
    next: Option[M[B, List[A]]]
fn need[T: Eq](x: T, y: T) -> bool:
    return x == y
fn go(m: M[int, Map[int, int]]) -> bool:
    return need(m, m)
```

`chezzi check` → **exit 137, OOM-killed at `MemoryMax=2G` in ~3 s**, from a 9-line file whose function
is never called. The pre-change binary refuses it in milliseconds. `editor::diagnostics` runs this walk
on every keystroke. A second prosecutor found the same class independently with
`Option[M[U, Option[T]]]`.

**Charge B (high) — a finite graph was refused, and the verdict depended on FIELD DECLARATION ORDER.**
Two *constant*-argument self-fields each reach a fixed point, but each was measured against the
previous sibling-induced ancestor rather than the root, so ascending `ty_nodes` counted as consecutive
growth:

```chezzi
struct W[T]:
    v: T
    a: Option[W[List[int]]]
    b: Option[W[Map[int, int]]]
```

`need(w, w)` at `W[int]` → *"W[int] nests too deeply"*; the pre-change binary and the rustc mirror both
accept. **Swapping the `a` and `b` declarations made the identical program compile** — verified. The
`EQ_BOUNDS_PROVEN` memo made it further path-dependent.

**The lesson, and it is the row's most valuable output.** Guard A was a *classifier*: it tried to
decide "is this type graph non-terminating?" from the shape. Across three landings it was wrong four
times, in both directions, each time green on the full suite, each time caught only by someone
building the adversarial neighbour by hand. So it was deleted rather than repaired a third time, and
replaced by a **direct bound on the resource that was the actual hazard** — a cumulative
`ty_nodes` budget over the in-progress path (`EQ_BOUNDS_MAX_NODES`), added on push and subtracted on
pop.

That change is not merely a better heuristic; it removes the failure modes structurally:

| how the classifier failed | why the budget cannot fail that way |
|---|---|
| evaded by permuting arguments | any walk whose types grow accumulates nodes, wherever the growth sits |
| evaded by a `Ty` variant nobody listed (`Ty::Func`, via a `_ => false` arm) | `ty_nodes` is exhaustive — a new variant is a compile error — and it is a *size metric*, not a shape test |
| order-dependent across sibling fields | the total is over the current PATH; pops subtract, so siblings never accumulate against each other |

**Measured on the release binary after the redesign** — refusals are cheaper than the old cap ever
managed, and every terminating neighbour is accepted:

| program | verdict | wall | peak RSS |
|---|---|---|---|
| argument permutation (Charge A) | refused | 38 ms | 22.6 MB |
| polymorphic recursion `Option[N[List[T]]]` | refused | 40 ms | 23.0 MB |
| growth through `Ty::Func` | refused | 48 ms | 26.3 MB |
| two constant siblings (Charge B) | **ok** | 3 ms | 13.6 MB |
| …same, fields swapped | **ok** | 3 ms | 13.6 MB |
| constant-arg growth, root `Wrapper[int]` / `Wrapper[str]` | **ok** / **ok** | 4 / 3 ms | 13.6 MB |
| shrinking re-entry `Pair[Pair[int]] → Pair[int]` | **ok** | 5 ms | 13.5 MB |
| 161-link chain (the depth 160 used to refuse) | **ok**, runs | 18 ms | 13.6 MB |
| 10 000-link conditional-`eq` chain | **ok** | 142 ms | 75.9 MB |

**Three further charges, all verified and all fixed in the same commit.**

*`checker::hover_type` was still on the caller's stack* (`src/checker/mod.rs`), the one driver of
`run_graph_pass` left unwrapped while four siblings were wrapped — relying on `editor::hover`'s
convention, which is exactly the hole this file already records biting once, when hover ran the whole
checker on a ~2 MiB LSP tokio worker. Now wrapped; all five drivers enumerated.

*The cap was in the wrong UNIT.* `EQ_BOUNDS_MAX_IN_PROGRESS = crate::vm::MAX_STRUCTURAL_DEPTH` claimed
the checker and the VM "literally cannot drift apart again". False: the checker counted in-progress
**Eq obligations**, the VM counts **value nesting depth**. `struct Node: v: int; next: Option[Node]`
has obligation depth 2 whatever the value's size, so a 20 000-deep `Node` is `ok: no type errors` and
then `runtime error: maximum structural depth (10000) exceeded` — the superset window was untouched.
The claim is withdrawn and the units are now stated apart.

*The `("Eq", "eq", "protocol")` ratchet row overstated its contract.* `s.eq(t)` on a `Shape`-typed
value is `type Shape has no method 'eq'` — correctly, by the object-safety rule. What the row grants is
`T = <some protocol>` satisfying `Eq` inside an erased `[T: Eq]` body; the wording now says so.

**A retired assertion, recorded rather than silently dropped.**
`a_long_chain_of_conditional_eq_types_is_decided_not_waved_through` used to end with "past the
backstop, even a SOUND chain is refused". Under a node budget that premise is gone: measured, that
chain checks clean at 10 000 / 16 000 / 25 000 links (135 / 240 / 353 ms), and widening the per-link
type does not change it (a 40-wide tuple per link at 1 500 links is still `ok`, 478 ms) because
`walk_eq_members` pushes a link's *members* and pops the wide tuple before walking them. That is the
designed behaviour — the budget exists to bound walks whose types GROW, and a non-growing graph is
precisely what this row was filed about the checker wrongly refusing. The refusal is therefore pinned
where it is reachable, on the growing shapes above, and the test now asserts the property that
matters: a 3 000-link chain (~19× the depth the original cap rejected) is accepted.

**Gate:** `cargo clippy --all-targets -- -D warnings` clean; `cargo test --lib checker::` **1808 passed
/ 0 failed**; `./target/release/chezzi test tests/chz/` **493/493 identical** on M:N and `--serial`;
all `examples/*.chz` check clean.

---
