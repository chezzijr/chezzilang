# Chezzi — gap backlog

Catch-all backlog of missing / shallow surface. **Not a commitment** — draw from it when a feature
earns its own milestone. Categories: **bugs** (fix, don't backlog), **root causes** (one change that
unblocks many gaps), **language**, **stdlib**, **IO/runtime**, **tooling/ecosystem**, **deps**.

**Audit history:** first stdlib pass 2026-07-07. **Full four-axis audit 2026-07-14** (IO/runtime,
stdlib breadth, language features, tooling) — that pass found one live data-corruption **bug**, three
cross-cutting **root causes** that were each recorded as unrelated footnotes, and a whole missing
**tooling** category. It also found the file's own #1 entry ("number format-specs") had been **shipped
and never de-staled**. Re-audit periodically: a gap backlog nobody re-reads rots into a to-do list for
work already done.

**Pre-freeze bug-hunt waves:** 1–5 (2026-07-11 → 07-13), then 2026-07-18, 07-20, 07-22, three on
07-23, and **wave 7 (2026-07-28)** — batch A swept the **host boundary** (the native/CLI seam where raw
OS bytes become Chezzi values): 3 findings, all FIXED, and it re-confirmed wave 6's meta-finding
(the panicking `std::env::args()` had three call sites, not the one the report named).
**Wave 6 (2026-07-25)** is the largest single haul (**19 findings**) and the first to sweep the two
surfaces the wave-5 residual named as never-audited (FFI: 4 defects; GC + new object layout: clean). As of
2026-07-28 all 19 + the 3 carve-outs are fixed, plus one follow-up found by adversarial review of the
W6-9 branch (**`W6-9b`**, the half-byte-exact parity oracle, fixed 2026-07-28); what remains are the
the one disclosed residual `W6-9r` — see the index below (`W6-10s` and `W6-10r` were the other two; both CLOSED 2026-08-06). Read its session log
before touching `io`/`process`/FFI/`RwShared`/module-snapshot code. Its meta-finding — **5 of 6 P0s are "a
fix applied to SOME arms of an N-way set"** — is the highest-yield remaining lever. **Wave 7
(2026-07-28)** is running against exactly that lever; `W7-3` (a `recover:` inside a cancelled task's
`defer` was bypassed) and `W7-4` (two sibling closures over one captured local got separate cells across
the airlock) were both instances of it and are **fixed** — session logs at the end of this file.

## OPEN ITEMS — the whole backlog at a glance (updated 2026-08-04)

Everything still open, roughly by severity. **No memory-unsafety is left in the ledger** — W6-8, the
last one, was fixed 2026-07-27 — and as of 2026-08-04 **nothing left in the ledger aborts the host
process**: W7-11, the last one, was fixed that day (`:4771`). Anything NOT listed here is either fixed or a safe-direction
observation. **Wave 7 batch A (2026-07-28) adds no row** — its three host-boundary findings
(`W7-1`/`W7-6`/`W7-7`) all landed FIXED; see its session log. Its deliberately-deferred sibling —
the **lossy path DECODE** in `fs.list_dir`/`walk`/`glob`/`canonicalize` and `os.getcwd` — was filed as
**W7-8** and is **FIXED 2026-07-31** (the `PathLike` protocol + `path.Path` type; see its session-log
section). The lossy-byte family now has **no unswept member**: B1, R1, W6-4, W6-9, W6-14 and W7-8 are
all closed. (`argv`/`env` remain a deliberately lossy surface — see `docs/stdlib.md`.) That sweep
covered what PROGRAMS emit; the same decode inside the **detectors** is its own sub-family and was
swept later — `W6-9b` (the parity oracle) and **`W7-30` 2026-08-07** (the CPython differential, which
outlives `--serial`). Both were byte-blind while every language surface around them was already
byte-exact, which is the standing lesson: *when a change widens what a program can emit, audit the
comparators in the same commit.*
**The Executor drain milestone (W7-5/W7-5b/W7-5c) is CLOSED 2026-08-03**: `W7-5` (run-all drain,
lowest-index-fault propagation) and `W7-5c` (every faulting task's output flushes) were fixed 2026-08-01
— see the W7-5 session-log section — and **`W7-5b` is FIXED 2026-08-03** by the eager-execution
milestone (`docs/future.md` §2c). Note the correction: eager execution did *not* dissolve W7-5b by
deleting the queue (it kept one). It dissolved it by changing what the program-exit join walks — a
heap-independent registry of `ExecutorCore` `Arc`s shared with every worker, instead of the per-`Vm`
`Vec<GcRef>` that died with its task's heap. **`W7-5d` is FIXED 2026-08-05** — a dead stdout is no
longer a hard halt, so the run-all drain covers it like any other ordinary fault, and **`W7-5e` is
FIXED the same day** — the counter that gate reads is now bumped inside the one door to streamed
stdout, so a *VM* write it cannot see does not compile. All five filed W7-5 rows are closed. Note the
scope: this is the VM's own sink. Bytes that reach fd 1 WITHOUT going through it — FFI calling libc
`puts`/`write` — are counted nowhere and are filed separately as **W7-20**.
**Keep this table in sync when a section is retired** — the
reason it exists is that "which of these is still open?" previously required reading 1400 lines of
chronological log.

| item | gaps.md | what | why it is still open |
|---|---|---|---|
| **W7-35** | `:7797` | `panicfuzz`'s subprocess harness has the identical bug `difftest` just fixed as `W7-34`: `run_one`'s `.spawn().ok()?` (`src/panicfuzz/run.rs:134`) and the staging `write_file` failure both collapse "the child never even started" into the same `None`/non-finding as an ordinary timeout, so `classify` reports `Outcome::Timeout` — a crash-detector sweep against a `chezzi_bin` that cannot spawn reports a clean pass instead of aborting | Filed, not fixed — out of scope for the `W7-34` fix pass per its own brief. `src/panicfuzz/` is a hand-maintained sibling of `src/difftest/` (deliberate copy, not a shared module — see its own header), so the `Result<Capture, RunErr>` + `Outcome::HarnessError` + caller-aborts fix needs its own pass, mirroring `W7-34`'s shape rather than reusing its code |
| `min`/`max` → `Option` | `:1690` | `List.min`/`max`/`min_by`/`max_by` fault on empty while `first`/`last`/`pop` return `Option[T]` | Breaking surface change: 23 call sites + docs + examples. Own milestone |
| `List[Any]` widening | `:1731` | `List[Any] = [1, 3.0]` silently widens the int to `1.0` | Deferred pre-freeze (wave 4) |
| **N10** | `:3456` | A `wait:` timer arm makes `--serial` inline-sleep instead of yielding to a runnable sibling (serial ≠ M:N) | Deliberate pre-freeze known-limit; fix is folded into the post-freeze serial-engine removal |
| ~~**W6-10s**~~ | `:1349` | `--max-heap` residual **sampling** escapes left after the byte-aware pacing fix | Pacing samples the cap on charged off-heap bytes, but only for stores routed through `to_wire_crossable` and only per heap. Still not sampled: the documented inline-scalar loop (`future.md §1b` — no `Obj`s, no wire bytes), the by-hand airlock paths (spawn args, closure captures, `Executor.submit`), and a heap that HOLDS a huge core without storing to it. **Premise partly re-derived 2026-08-06 and it did NOT hold**: the `Executor.submit` arm is the only one of the three that stores persistently off-heap, and it does so ONLY on `--serial` (M:N executes eagerly and queues nothing) — which `--max-heap` refuses at the CLI (`main.rs:685`), so that arm is unreachable. The spawn-arg / capture arms are transient: `prepare_worker` rebuilds them into a worker heap immediately. What IS reachable was measured instead and is a different mechanism: a worker heap **born big** — `spawn use(blob)` with a 200 000-int `blob` PASSED `--max-heap=8000000`, and adding ONE allocating statement to the spawned fn turned the same program OVER-MEMORY at the same RSS, because the payload arrives in ~7 objects so `since_gc` never reaches `next_gc` and `sweep()` — the sole assigner of `over_cap` — never runs. Charging bytes in `Heap::alloc` does NOT fix it: these containers are alloc'd EMPTY and patched via `get_mut` (the tie-the-knot rebuild), so there is nothing to charge at alloc time. **FIXED 2026-08-06** by `Heap::request_collect`: `Vm::spawn_worker` — the one door every worker heap is born through, and where the cap is already threaded — asks for the first collect whenever a cap is live, and the flag is consumed at the task's first instruction boundary in `run_until` (the first properly-rooted point; it is set before the payload is rebuilt, which is safe because `Heap::alloc` never collects). `spawn use(blob)` with `use = return xs.len()` at `--max-heap=1000000`: **PASS → OVER-MEMORY**, with the generous-cap, no-cap and 50-spawn controls all still PASS. **Two residuals stay open and are NOT claimed fixed** (both measured, neither introduced by the fix): (a) a task whose ENTIRE body is one native call (`spawn blob.len()`) executes no bytecode, so it reaches no instruction boundary and the flag is never consumed — and there is no safe sample point in that window, since the payload is rooted only as the pending call's operands (collect before the call and you free the task's own arguments; after it the receiver is already dead); (b) growth AFTER the sample that allocates no `Obj`s — `xs.push(i)` grew a worker heap 32× past the cap post-sweep and never re-triggered, which is `future.md §1b`. So the fix narrows "the verdict tracks who allocates" to bytecode-running tasks; it does not make `--max-heap` total. **Residual (b) is CLOSED 2026-08-07 by `W7-28` (`:7158`)** — and its filed price was an understatement: not 32× but UNBOUNDED (80 M `push`es rode to **617.8 MB, 77×** an 8 MB cap), and the same class covers `Map`/`Set`, one-instruction `extend` (~240 MB), and few-allocation/huge-byte growth (`s = s + s` ×22 = 41 MB, `"x".repeat(20000000)` = 20 MB in ONE alloc). Fixed by charging BYTES at the three funnels a heap gains them through, after an instruction-tick first attempt shipped green and still let `extend` past. **Residual (a) is CLOSED 2026-08-07 by `W7-29` (`:7279`)** — `W7-28` first re-scoped it (every shape that used to demonstrate it began tripping on the PARENT, which built the payload and now samples its own bytes), leaving the narrower case of a worker heap over the cap while its parent's is under; `W7-29` built that repro and fixed it. Both this row's residuals are now closed |
| ~~**W7-29**~~ | `:7279` | **FIXED 2026-08-07 — closes `W6-10s` residual (a), which had been filed as having "no safe sample point".** A spawned task whose ENTIRE body is one native call pushes no frame, so `run_until`'s `while self.frames.len() > base_level` never runs an iteration, `should_collect()` is never called, `sweep()` never runs and `over_cap` is never assigned. Two programs with byte-identical payloads (~171 MB) against an 8 MB cap, differing ONLY in the task body: `spawn xs.len()` **PASSED at 170.9 MB (21×)** while `spawn use(xs)` reported OVER-MEMORY — **the verdict tracked who ran BYTECODE, not who held bytes.** Fixed by `Vm::sample_mem_cap` called from `Vm::start_task` under a live cap: the `Method` arm already has receiver+args on the operand stack (which `Vm::collect` traces first) so a direct `collect()` is sound with `frames` empty — empty frames end `run_until`'s LOOP, they do not make a collect unsound; the `Call` arm parks callee+args on the stack across the sample and takes them back, since a `Callee::Builtin`/`Native` callee pushes no frame either | **Masked for three rounds by an unrelated implementation artifact.** `do_spawn` deep-clones the payload into the SPAWNING heap first, and on the lazy nursery path that copy stays rooted in `self.nurseries` until the join — so the PARENT tripped and every repro looked guarded. Proved by a doubling test: `nospawn` flips at ~2.7 MB, `spawn` at ~6.5 MB — 2×, which only a second full copy in the parent explains. Second: the first cut DELETED `Heap::request_collect` as subsumed, and **both prosecutors independently caught** that `ReadyWorker::invoke` (the eager-`Executor` job door) never routes through `start_task`. Restored — two doors, each documenting what it owns; "no witness built" is not "unreachable". Third: the new test **could not fail on its own regression** — it was green on a single core with the fix reverted, and `CHEZZI_THREADS=1` does not show this (that var is read by `cmd_run`, not the test helper) — only `taskset -c 0` does. It now CONTROLS the environment (`set_worker_count(4)` behind an RAII guard) instead of asserting it |
| ~~**W7-28**~~ | `:7158` | **FIXED 2026-08-07 (closes `W6-10s` residual (b), plus two siblings never filed).** `--max-heap`'s sample trigger counted EVENTS — `Obj` allocations and off-heap wire crossings — and every event class has a shape that adds unbounded bytes without raising it. Against an 8 MB cap: `xs.push(i)` × 80 M appends into the `Vec` behind an existing `Obj::List` and moves nothing (**PASS at 617.8 MB, 77× the cap** — filed as "32×", in fact unbounded); `big.extend(chunk)` × 150 does it in ~1200 instructions (**PASS at ~240 MB**); `s = s + s` ×22 (41 MB) and `"x".repeat(20000000)` (20 MB in ONE allocation) stay under the 256-object floor. `Map`/`Set` fail open like `List`; `str` concat did NOT, which pins the mechanism exactly. The accounting was already right (`bytes_in` charges `Vec::capacity()`), so all of it was pure non-observation. Fixed by charging BYTES at the three funnels a heap gains them through — `Heap::alloc` (the only constructor), `Heap::get_mut` (the SOLE `&mut Obj` door — a deferred before/after delta settled at the next door or the next `should_collect`, so a new container method cannot re-open the hole) and the existing `Vm::to_wire_crossable` — with one `obj_bytes_shallow` sizing table shared with `bytes_in`, core arms scoring 0 and taking NO lock (the `own_bytes` self-deadlock lesson). All shapes **PASS → OVER-MEMORY, rc=1, 11–32 MB**; generous-cap controls still PASS at full footprint | **A first fix shipped fully green and was still wrong, and only adversarial review caught it.** Round 1 added an INSTRUCTION TICK on the premise "an instruction can grow the heap by at most O(1) bytes" — full suite, both engines, clippy, and the `push` repro all green, and it still let `extend` put 240 MB past an 8 MB cap, because one instruction appends N values. **A proxy that is only *nearly* proportional to bytes is not a byte counter, and the gap is exactly where the bug lives.** Second: the same review found the re-based control confounded — it used `for i in range(200000)`, and `range()` materialises its own 1.6 MB `List[int]` before a single `push`, so the assertion passed with the loop body replaced by `pass`. Third: the fix costs a measured **~+1% on `struct`** (the alloc-heaviest bench, reproduced 3×, same sign) for one `mem_cap != 0` branch in `alloc` — recorded rather than argued away |
| ~~**W7-27**~~ | `:6971` | **FIXED 2026-08-06 (found by adversarial review of the `W7-26` fix; premise re-measured on the release binary before any edit).** An `Executor` job's RETURN VALUE was retained until `shutdown()` even though nothing can ever read it — `submit` returns nil (no futures), `WorkerResult.value` is `#[allow(dead_code)]`, and `reduce_task_slots` reads only `out`/`stderr`. 300 × `ex.submit` of a job **building** its own ~1 MB str, results discarded, uncapped `chezzi run`: **peak RSS 339 MB → 45 MB**, against CPython 3.14.6 `ThreadPoolExecutor`'s **42 MB** (futures discarded identically, `resource.ru_maxrss`) — an ~8× drift closed to parity, and it needed NO cap to bite. One line in `ReadyWorker::run_outcome`: the `Done` outcome stores `WireValue::Nil`, exactly like the M:N nursery path. `to_wire_at` + `ensure_crossable` still run, and dropping their product does not make them dead: `to_wire_at` is FALLIBLE, so a return value that cannot cross (a generator closing a reference cycle, a depth/size cap) still faults at the submit site with the task's real span — the fault contract is the crossing, not the storage. `W7-26`'s accounting stays: `out`/`stderr` are unbounded on their own | **RETENTION and ACCOUNTING are separate bugs on the same bytes, and fixing one re-bases the other's test.** `W7-26` made these bytes visible to `--max-heap`; this frees them — so its M:N repro stopped overshooting and `over_memory_counts_an_executor_result_backlog` was re-based onto buffered OUTPUT (300 jobs printing ~100 KB), which is what the eager accounting still has to count. The old return-value program became the inverse fence, `executor_results_are_not_retained`: same 8 MB cap, must now PASS. **Residual, filed on `W7-26r` and fixed there the same day:** the CAPTURED-blob variant is still **410 MB** of RSS (was 666 MB) — each `submit` builds its own ~1 MB copy of the capture into a `ReadyWorker` that queues in the process-global pool. CPython holds 17 MB there because a captured `str` is shared by reference, which Chezzi's by-value airlock cannot do, so that RSS is an isolation-model cost; `W7-26r` closed the accounting half, charging those copies to the submitter so the cap can see them |
| ~~**W7-26r**~~ | `:6929` | **FIXED 2026-08-06, both halves (the join residual AND the pool-queue sibling), premises re-derived on the release binary first.** `--max-heap` was never observed while the parent fiber sat in a join — `over_cap` is assigned only in `sweep()`, which runs only at the parent's own instruction boundary, and a parent inside `Executor.shutdown()` or a `parallel:` join reaches none. Against an 8 MB cap: an executor whose 300 jobs each buffer ~1 MB of output **PASSED at 622 MB**, and the same shape under `parallel:`/`spawn` **PASSED at 733 MB** — the nursery half counted NOWHERE at all, its outcome slots living in `SchedCore::slots`, outside every `Heap`. **Both owning ancestors put the observation on the ALLOCATOR, never on the blocked consumer** (measured, not reasoned): CPython 3.14.6's `ThreadPoolExecutor` under a 300 MB `RLIMIT_AS` raised `MemoryError` **in the worker at job 57/500** while `main` sat in `ex.shutdown()`; Go 1.26 under `GOMEMLIMIT=32MiB` + `GOGC=off` ran **7 GC cycles while `main` was blocked** in `wg.Wait()`. So the fix is `core::halt_over_backlog`, called by the two producers (`dispatch_eager_job`'s pool closure and `MnSched::finish`): the thread that finished a task adds its outcome to the join's retained-byte total and, if that backlog ALONE exceeds the whole cap, replaces its own `Done`/`Cancelled` with a hard-halt over-memory `Fault` and trips its scope/core cancel. Everything downstream is reused — `reduce_task_slots`' `Exit > hard-halt > ordinary` precedence propagates it and `recover:` cannot catch it. It cannot false-positive: the trip needs the retained backlog by itself to exceed the cap, and those bytes are held until the join reduces. **PASS → OVER-MEMORY on both, generous-cap and 300-tiny-spawn controls still PASS.** The SIBLING (queued-but-not-started jobs) is the same story one layer out: `prepare_eager_job` rebuilds each submitted closure into its own worker `Vm` at submit, so a queue deeper than the pool is N whole worker heaps — each under a per-heap cap, summing to **666 MB past an 8 MB cap** — now owned by the submitter (`ExecutorCore::pending`, added at dispatch, removed the instant a pool thread takes the job, so it never double-counts the running heap's own charge) | **Counting them is worthless if nothing looks — for the THIRD time in this family.** With the sibling's accounting alone the 666 MB program still PASSED: a loop submitting slow jobs finishes none of them, so `take_charge` stays 0 and the parent (which allocates almost nothing per submit) never sweeps. Fixed by pacing on the same bytes at the same site. Second lesson, from the review that made this test honest: the obvious control — same 1 MB capture, fast job body, assert the pool keeps up — **FAILED under full-suite load**, because a busy machine really does let ~48 jobs queue and ~48 MB really is live, so the trip was CORRECT and the assertion was wrong. A cap verdict is load-dependent by nature (both ancestors are too), so the control had to shrink the PAYLOAD rather than rely on timing. Filed then as still open, BOTH now closed: the general `W6-10s` residual (a) — a heap that grows for reasons OTHER than a join backlog while its fiber is inside one native call — fixed by **`W7-29`** 2026-08-07 (`Vm::start_task` samples before dispatch); and per-heap containment is not per-process containment, closed 2026-08-07 as BY DESIGN after measuring both ancestors (CPython's `RLIMIT_AS` aborts, Go's `GOMEMLIMIT` does not — they disagree, and Chezzi's PASS matches Go), so a nursery of N tasks each individually under the cap still peaks high (reduction-count preemption interleaves all N payload builds) even though the verdict is now right. Third: the filed premise had DECAYED and the shape had to be rebuilt before the fix — the row's own repro was return-value-based, and `W7-27` (landed hours earlier) frees those, so every repro here is output-based instead |
| ~~**W7-26**~~ | `:6871` | **FIXED 2026-08-06 (found by adversarial review of the `W6-10r` fix, premise re-derived on the release binary before any edit).** `--max-heap` read only the `Executor` core's `inner` QUEUE half, which is the `--serial` half: on the default M:N engine `submit` runs EAGERLY (matching `ThreadPoolExecutor`), so `inner` stays empty forever and every finished job's result lands in `eager.slots` as `TaskOutcome::Done(WorkerResult { value, out, stderr })` — reached by `live_bytes` nowhere. `Executor()` + a ~1 MB blob + `300 × ex.submit(fn() -> str: blob)` + `shutdown()` under `--max-heap=8000000`: **PASS, rc=0, peak RSS 313 MB → OVER-MEMORY, rc=1, 11 MB**; generous-cap (4 GB) and no-cap controls still PASS. `EagerState` gained the `(bytes, dirty)` summary `ChanState`/`ExecState` already carry, maintained in `finish`/`take_slots` over a PRIVATE slot vector (so no site can forget), and the `live_bytes` arm now sums BOTH halves. The charge is **unconditional**, unlike the `mem_cap != 0` gates elsewhere in this family: it fires once per finished job beside a thread handoff, and both ancestors keep accounting live with the *limit* separate (Go's `runtime.MemStats` vs `GOMEMLIMIT`). Rooting unchanged and now FENCED rather than reasoned about — a result crosses by value with no parent-heap `GcRef` (B3.2, enforced by `ensure_crossable`), asserted by a `debug_assert!(!w.has_handle())` in `outcome_summary` that says `Heap::children` needs an eager arm if it ever fires | **Counting is worthless if nothing samples, and the fix's first cut proved it twice.** With the accounting alone the repro tripped at **180 MB**, not 11 MB: `submit` wires the closure but nothing charged the RESULTS against the parent's GC pacing counter, so the parent swept only on its own `Obj` growth — closed by `EagerState::take_charge` (growth-since-last-read, charged at `submit` under a live cap). A SECOND shape survives even that and is filed below as an open residual, because it is the W6-10s sampling class rather than this one: a job that BUILDS its own payload (`ex.submit(mk)`, no capture) wires ~nothing at submit and all 300 submits complete before any job finishes, so `take_charge` sees 0 each time and the whole 330 MB accumulates while the parent is blocked inside `shutdown()`'s join — where there is no sample point at all. Adding ONE allocating statement to the parent turns that same program OVER-MEMORY (rc=1), which is what proves the accounting half is right and the gap is purely observational |
| **W6-9r** | `:1473` | Parity-oracle residual left by the `W6-9b` fix: hand-rolled `run_file_p` + `run_file` cross-engine compares still diff LOSSILY-DECODED strings, and `parity_entry_cfg_lines` compares stdout as an order-insensitive line multiset | **Item 1 CLOSED as WON'T FIX 2026-08-07; items 2–4 open on their existing terms.** Scope was under-recorded: not "~31 in `parity_tests.rs`" but **~60 sites** across `parity_tests.rs` + `src/vm/tests.rs` + `src/native/cffi.rs` (e.g. `tests.rs:8180`/`:8183`, `cffi.rs:2280`). Not worth converting, because **these compares die with `--serial`**: `future.md §2b`'s migration mechanics turn each pair into a single-engine (M:N) GOLDEN test, and a golden compares against a UTF-8 literal — where "a decode cannot hide anything when the other side is a UTF-8 literal" (item 3's own finding). UTF-8-only today, so nothing is failing; a byte-emitting test added at one of those sites must use `vm::run_file_bytes`. What the re-derivation DID find is the same hole in the oracle that OUTLIVES `--serial` — the CPython differential — filed and fixed as `W7-30` |
| ~~**W7-37**~~ | `:7938` | **FIXED 2026-08-07 (found by an audit of what the CPython differential GENERATES, the last unexamined half of the oracle after `W7-30`–`W7-36` audited what it REPORTS).** `gen_float` ignored its `depth` and always returned an `n/8` literal that both emitters rendered from the same shared `float_lit`, so `Expr::Bin { ty: Ty::Float }` was **never generated** and `emit_python`'s float fall-through was dead code; `Features::full()` had `floats: false` on top of that. Separately `try_call`/`try_index` were only ever asked for `Ty::Int`, so ~2/3 of generated functions were emitted and **never called** and non-int element reads never happened. Both engines "agreed" on all of it because neither executed it | **Coverage that both engines agree on because neither runs it is not coverage — and the proof is `W7-32`.** That was a real float-repr bug (shortest-repr ties rounding away from zero) in exactly the formatting seam this oracle owns, and the oracle could not have found it: a hand-written differential did. `gen_float` is now recursive over `Add`/`Sub`/`Mul` with float vars, `try_call`, and `try_index` leaves; `gen_bool`/`gen_str` gained the same call+index attempts; float vars became assignable; `Features::full().floats` is now `true`. Leaves are `n/8` **scaled by a power of two** — exact (a power of two moves only the exponent field), and the scale is what actually reaches the sci-notation crossover the brief assumed a mul chain would reach on its own (it does not: `n/8` tops out near `1e8`, the crossover is `1e16`). Deferred, in order: float **comparison** (`gen_bool`'s comparison arm calls `gen_int` for BOTH operands, so `<`/`<=`/`==`/`!=` on floats is still never generated — measured 2026-08-07, `gen_bool` mentions `Ty::Float` nowhere; same species as the hole this row closes, and the cheapest of the three), float `Div` (a zero divisor raises `ZeroDivisionError` in CPython; inexact quotients widen the formatting surface at once), and int↔float mixed arithmetic |
| ~~**W7-34**~~ | `:7730` | **FIXED 2026-08-07 (found by the same audit as `W7-33`).** `difffuzz --seeds 0..50` with `chezzi` not on `PATH` printed `done: 50 seeds, 0 finding(s) [(0, 50)]` and exited 0 — `run_one` collapsed "could not even spawn the child" into the same `Option::None` as an ordinary timeout, and `run_sources`/`write_file` mapped it to `Outcome::Timeout`/`BothError`, both non-findings | `run_one` now returns `Result<Capture, RunErr>` (`TimedOut` / `CouldNotRun(String)` carrying the real `io::Error` text); a new `Outcome::HarnessError(String)` (`is_finding() == false`, but FATAL to the two callers — `fuzz_range` panics on the first one instead of accumulating it, `difffuzz` exits **2**, distinct from **1** for real findings). `Capture::code`'s "`None` = signal kill only" invariant (`W7-33`) is unchanged. Re-measured post-fix: `env PATH=/usr/bin:/bin ./target/release/difffuzz --seeds 0..50` now prints `harness error at seed 0: could not run "chezzi": No such file or directory (os error 2)` and exits 2 |
| ~~**W7-33**~~ | `:7649` | **FIXED 2026-08-07 (found by an audit of the whole CPython-differential oracle for the "real bug reported as a non-finding" class, prompted by `W7-31`).** The CPython differential's `classify` never examined `chz.code.is_none()` (signal kill: SIGSEGV/SIGABRT/a Rust stack overflow) ANYWHERE, and its first arm (both exit 0, compare stdout) never consulted `is_host_panic` at all — so a signal-killed chezzi was an ordinary `ChezziFault`/`BothError` non-finding, and a **worker thread** panicking on stderr while `main` exits 0 with matching stdout was `Match`. The twin oracle one directory over, `panicfuzz::classify`, already had the `code.is_none()` rule (`src/panicfuzz/run.rs:98-100`) — this was a divergence between two sibling oracles, not a novel design question | The proximate cause: `Capture.code`'s own doc comment read `// None => killed by signal / timeout`, and the "/ timeout" half is false (a timeout returns `None` from `run_one` before any `Capture` is built), which is plausibly why nobody classified the signal-kill case. Fixed by moving BOTH checks (`is_host_panic`, then `code.is_none()`) to the top of `classify`, before any arm-specific logic and before any `allowlist::check` call, so they run unconditionally on all three arms |
| ~~**W7-32**~~ | `:7553` | **FIXED 2026-08-07 (found by re-deriving `W7-31`'s premise — the dead allow-list entry pointed at the right neighbourhood for the wrong reason).** A **real language bug**, not a detector one: `repr_float` took its shortest-repr digits from Rust's formatter, which breaks an EXACT half-way tie **away from zero** where CPython breaks it **to even** — `print(771.5462036132812)` gave `771.5462036132813` (exact value `771.54620361328125`). Moved `str`/`print`/interpolation/`json.stringify`. Not the lexer: both sides parse the literal to the same `f64` | 20 000-value fuzz vs CPython 3.14.6 found 6, all one shape. Fixed by *reusing Rust's own exact half-even `{:.N}` formatter* at the shortest digit count instead of hand-rolling digit surgery — plus a **round-trip guard**, which is load-bearing: at a binade boundary (`2^-24`) the even candidate names a different float, so CPython keeps the odd digit too. A version without that guard passed the whole suite AND the 5 400-value differential; only a 60 000-value tie-rich `m/2^k` fuzz caught it. 213 791 floats now byte-identical to CPython |
| ~~**W7-31**~~ | `:7468` | **FIXED 2026-08-07.** The CPython differential's float-formatting allow-list looks only at the two stdouts, never at `code`, but `classify` reaches it from THREE arms — so Chezzi printing `1e-05` and then FAULTING (exit 1) while CPython prints `0.00001` and exits 0 is downgraded to `AllowListed`: a Chezzi crash reported as a non-finding | **Pre-existing** (same three call sites since `95fbbd5a`), surfaced by adversarial review of the `W7-30` branch while checking an unrelated claim. A real Rust host panic is still safe (`is_host_panic` returns first). Filed as a per-MATCHER gate, shipped as a **deletion** instead — the premise was re-derived on the release binary and found dead. Not folded into `W7-30`: different bug, changed what the oracle REPORTS, needed its own failing-first test + corpus re-run |
| ~~**W7-30**~~ | `:7408` | **FIXED 2026-08-07 (found by re-deriving `W6-9r` item 1 rather than trusting its filed price).** The CPython differential — the oracle `future.md §2b` keeps after `--serial` is deleted — read raw `Vec<u8>` off the child's pipe and threw it away one line later (`run.rs:253`), then compared the decoded `String`s (`:129`). `from_utf8_lossy` is not injective, so a run where Chezzi wrote `ff fe` and CPython wrote `fe ff` was `Outcome::Match`. Both sides can emit non-UTF-8 (`write_bytes` since W6-9; `sys.stdout.buffer.write` always) — measured, CPython / Go / `chezzi run` all put `ff fe` on fd 1, so the runtime was right and only the detector was blind. Fixed by making `Capture.stdout`/`.stderr` `Vec<u8>`: `classify`'s existing compare needed no edit and is now byte-exact, and the blind version is **unrepresentable**, not merely fixed. `stdout_text()`/`stderr_text()` (`Cow<str>`) serve the three text consumers — `is_host_panic`, the float-formatting allow-list matcher (which only runs after the byte compare already found a difference), and the report | **A residual's filed price decays, and re-deriving it can move the fix somewhere better.** `W6-9r` item 1 read as ~31 sites of unavoidable churn; measuring found ~60 — and found that all of them are scheduled for deletion, which turns a 50-line newtype into wasted work. The same 20 minutes located the identical hole in the oracle that is *not* scheduled for deletion. Second: `describe` had to grow a hex line for the byte-only case, or the report would show a `Divergence` verdict over two identical-looking stdout blocks — **a detector that is right and unreadable teaches the same distrust as one that is wrong** |
| ~~**W6-10r**~~ | `:1390` | `--max-heap` residual: a payload reachable ONLY through a **nested** core (a `Channel` inside a `Shared`, once the nested core's last `Obj` alias slot is swept) is counted nowhere | **Premise re-derived and CONFIRMED 2026-08-06** before any edit (the two preceding rows in this family had premises that had gone stale): `make() -> Shared[Channel[str]]` parking a channel whose only alias slot dies with the frame, then 300 × ~1 MB `s.get().send(blob)` → **PASS, rc=0, peak RSS 304 MB** against `--max-heap=8000000`, while the identical program holding the channel in a live local tripped OVER-MEMORY. **FIXED 2026-08-06**: `core::nested_core_bytes` — the byte mirror of `collect_core_gcrefs`, which already recursed into nested cores for ROOTING (only the byte walk stopped at the boundary) — plus `queue_bytes_deep` / `value_core_bytes_deep`, called from `Heap::live_bytes`. The recursion shares `live_bytes`'s per-heap `Arc`-identity set, so a nested core that also has an alias slot here is still charged exactly once, and it fills a `WS_UNKNOWN` summary in passing (every core constructor leaves it UNKNOWN, and a core reached only through a parent is never marked through a slot of its own — without the fill it reports 0 forever). Gated on `mem_cap != 0`, the same argument as the round-3 pacing counter: cap-off runs (every `chezzi run`, every bench, the whole parity gate) pay one `!= 0` load and ZERO extra walks. Repro: **PASS @ 304 MB → OVER-MEMORY, rc=1, 16.5 MB**; generous-cap and no-cap controls still PASS. Tests `vm::heap::live_bytes_counts_a_nested_core_with_no_alias_slot` (cap-off unchanged / cap-on charges / alias-slot de-dup) + `test_runner::over_memory_counts_a_nested_core_backlog` (both engines), both mutation-verified red with the walk disabled |
| ~~**W7-25**~~ | `:6725` | **FIXED 2026-08-06 (breaking output change).** A string nested in a container / struct field / enum payload rendered RAW, so different values printed identically: `["a", "b"]` and `["a, b"]` were both `[a, b]`, and `[""]` printed `[]` — `str(a) == str(b)` true while `a == b` false. Now Python `repr` (`slice::str_repr`, cross-checked against CPython 3.14), applied by `stringify_nested_into` at the six nesting sites; a `str(self)` hook's own result is deliberately NOT quoted. Sweep: 14 goldens, 15 Rust expectations, 8 chz assertions | **The detector encoded the bug.** The CPython differential oracle's shim defined `_chz_repr(v) = v if isinstance(v, str) else _chz_str(v)` — it mirrored the raw-nested-string behavior, so the one tool built to catch a Python divergence could never report this one. Eight difftest suites went red when the implementation was fixed. **A detector written to mirror the implementation is blind to bugs in what it mirrors** |
| ~~**W7-24**~~ | `:6660` | **FIXED 2026-08-06.** Call-argument normalization never reached an interpolation fragment: `"{f(1)}"` with `fn f(a: int, b: int = 2)` was `'f' expects 2 argument(s), got 1`, `"{sub(y=1, x=10)}"` was `got 0`, and `"{sum_all(1, 2, 3)}"` was `expects 1 argument(s), got 3` + `expected List[int], found int` — every one of them correct outside the string. `ExprKind::Str` held RAW text, so `desugar::run` (named args + defaults + variadic sweeping, one pass) ran before the fragment was parsed, and three separate consumers re-parsed it after. Fixed by `ExprKind::Interp(Vec<Chunk>)`, produced by `desugar` itself — fragments become real children before normalization, inside the live scope stack | **An invariant a pass establishes only holds for what that pass can SEE.** The checker received exactly the `Call` shape desugar's own header promises it never will (`named` non-empty, defaults unfilled), which is why the errors were incoherent rather than merely wrong. Raw text stored in an AST node is a hole in every tree-walking guarantee, and the hole only surfaces at the consumer |
| ~~**W7-23**~~ | `:6615` | **FIXED 2026-08-06.** The interpolation fragment scanner was neither quote- nor depth-aware: it cut at the FIRST `}`, so `"{d['a}}b']}"` was `unmatched '}' in string` and `"{ {1, 2}.len() }"` was `unexpected an indented block in expression` — valid code, hard compile errors. Now it carries `fmtspec::split_spec`'s own `in_str` + bracket-`depth` state. Also: a fragment is lexed as its own line, so leading padding (`"{ 1 + 2 }"`, legal in CPython) opened an INDENT token — `parse_expr_str` now trims | **One layer was careful about quotes and the layer feeding it was not** — `split_spec` is called on the very NEXT line of the same function and has been quote-aware since it shipped. A shared invariant implemented in one of two adjacent layers reads as implemented in both |
| ~~**W7-22**~~ | `:6076` | **FIXED 2026-08-06.** Every container crossing the airlock was rebuilt with **22× the capacity it needs**, and kept it for the object's whole lifetime. Fourteen wire→`Value` rebuild sites were `items.into_iter().map(…).collect()`, which Rust specializes into an **in-place** collect: the destination element is smaller than the source, so the source `Vec`'s allocation is REUSED and the rebuilt `Vec<Value>` inherits its capacity — `size_of::<WireValue>() / size_of::<Value>()` = 176 / 8. Measured on the release binary: a 200 000-int list crossing a `spawn` arrived as `len = 200 000, capacity = 4 400 000` (a **35.2 MB** `Obj::List` holding 1.6 MB), halving the list halved the capacity to 2 200 000 — exactly 22× both times. 50 such spawns: **peak RSS 3.45 GB → 203 MB**. Hits `spawn` args, `Channel.recv`, `Shared.get`/`RwShared` reads, closure captures, struct/enum/generator payloads. A capturing `spawn` measured the same: **3.45 GB → 203 MB**. Fix: one `Vm::rebuild_items` helper (pre-size, then push) at all fourteen sites | **`len` is identical either way, so the entire behavioural suite is blind to it** — 3856 tests, both engines, byte-identical output, all green before and after. Only `Vec::capacity` can see it, which is why a memory bug of this size survived every wave of the bug-hunt: every existing assertion is about *values*, and this changes none. Second: it was found while chasing a DIFFERENT filed row (`W6-10s`), by asking why a repro's RSS was 22× what arithmetic predicted rather than accepting that the cap "failed open" — the filed row's own premise (a serial-only `Executor.submit` escape) turned out to be unreachable, because `--max-heap` refuses `--serial` at the CLI. Third: **the first cut fixed eight of the fourteen** — `from_wire_memo`'s container arms — and left the identical shape in `deep_clone_all` and `rebuild_ready`'s five `Lowered` arms, two of which feed a DURABLE `Obj::Closure { captured }`. So `spawn` with a capturing closure still leaked 22–24×, under a new doc comment asserting captures were fixed, with the full suite green. Adversarial review caught it; both prosecutors filed it independently. *This is wave 6's meta-finding — "a fix applied to SOME arms of an N-way set" — reproduced by the fix for a bug found while auditing that very class.* Fourth: the fix is invisible to `benches/run.chz`, which is single-threaded and never crosses the airlock — a whole class of regression the bench set cannot price |
| ~~protocol embeds~~ | `:6365` | **FIXED 2026-08-06.** A protocol's embed set is now flattened at EVERY use site, not just at a bound. `p: Person` → `p.name()` (embedded) went `type error … type Person has no method 'name'` → `ada 36`; the same through a bound, `<` through an embedded `Comparable`, `in` through an embedded `Contains`, and passing a `Person` value to a `Named` parameter all went type-error → correct value. Both ancestors were re-run and agree: Go (embedded interface + interface-to-interface assignment + a generic constraint) prints `ada 36` / `ada` / `eve 7`; pyright is clean on the `Protocol`-inheritance twin. Checker-only — the runtime dispatches every one of these by NAME already. **Bounded by object safety**: a method taking `Self` (so every operator protocol, whose method is `(self, Self) -> Self`) stays BOUND-only, because a protocol value erases which witness it holds | **The CONTROLS are what made this two bugs instead of one.** Running each case with an OWN (non-embedded) method split the report cleanly: five cases passed the control and so were the embed bug; `a + b` on two protocol values and a `Self`-typed method on an existential FAILED their controls too — and "fixing" those two opened a SOUNDNESS HOLE that shipped FULLY GREEN (3848 tests, clippy, both engines) before adversarial review caught it: `plus(V(1), W("q"))` through `fn plus(a: Vecish, b: Vecish)` checked ok and faulted `no field 'x' on W`. A protocol value erases its witness, so `Self` in a parameter slot is bound-only — Rust's object-safety rule, and why Go bans `Self` in interfaces outright. The licence taken for it was a pyright PASS; but Python's `Protocol` is GRADUAL and enforces no witness identity, so it was never evidence a statically-enforced language may accept it. Two more, same review: the embed walk had a depth cap and no VISITED SET (branching ≥2 ⇒ 2^64, so a diamond/cyclic graph hung `check` on a method miss), and the CONFORMANCE half of the embed-arg re-spelling was left on the old resolver, so `b: PBag[str] = B` (a `contains(self, int)`) was accepted. Second lesson, found by the negative test: the flatten's own `resolve_ty_ro` reads `self.type_params`, which at a USE site is the *calling function's* params — so `protocol Bag[T]: Contains[T]` resolved its `T` to `Unknown` and `"x" in b` type-checked on a `Bag[int]`. A widening's negative control is not paperwork; it is the only thing that catches a widening that went permissive |
| ~~**W7-21**~~ | `:5835` | **FIXED 2026-08-05.** A module global holding a FN VALUE is now CALLABLE through the module: `l.BARE()` went `type error … module 'l' has no member 'BARE'` (rc=1) → `ok`, and prints `1` on M:N, `--threads=1` and `--serial`. Both ancestors agree and were re-run: CPython `pk.G()` → `1`, Go `pkg.G()` → `1`. Checker-only — the `Ty::Module` call arm now falls back to `sig.values` and calls a `Func`/`BuiltinFn` there with STRICT `check_args`; the compiler's `Op::CallMethod` fall-through and `Obj::Module` dispatch already handled it. The lying diagnostic is fixed too: an existing-but-uncallable member says `member 'N' is not callable (it has type int)` | **The obvious runtime test was green BEFORE the fix, and that is the lesson.** For a `checker⊋compiler` sibling (checker rejects what the system executes) the instinct is "run it on both engines" — but `run_file`/`run_file_parallel` bypass the checker, so the both-engine test passes pre-fix. It proves the *lowering* exists; only a graph-level `check_graph` test proves the *rejection* is gone. Two claims, two tests, neither substitutes for the other (the VM test's doc-comment now says which one it is). Second: `from l import BARE` + `BARE()` always worked, which is precisely what kept the qualified arm the single broken site — a member surface harvested into two maps with one consumer reading one of them |
| ~~**W7-5d**~~ | `:5188` | **FIXED 2026-08-05.** A dead stdout was a whole-queue kill switch, so a broken pipe cancelled sibling `Executor` jobs. It is now an ORDINARY per-job fault. TWO process-global reads had to go: `executor_hard_halt` is `is_over_memory \|\| is_timed_out`, and `invoke_native`'s post-call `stream_halt` is gated on that call having actually emitted to stdout (`Vm::stdout_writes`) — unguarded it faulted a sibling that never printed, after its FIRST native. Repro: **both markers 21/21 runs**, **all three writes 15/15**, across `--serial` and `--threads=1/2/3/4/8`/default; pre-fix it wrote neither marker at `--threads=1`/`--serial`, both at `--threads=3+`, and either at `--threads=2`. Matches CPython `ThreadPoolExecutor` (`max_workers` 1/2/4), the ancestor that owns `Executor` | Three lessons. (1) A **process-GLOBAL read inside an error-property predicate** — `out_dead_reason().is_some()` does not describe the `err` it is passed. The shape was in the tree **twice**; fixing the first made the second visible. (2) The ledger's own proposed alternative ("an accepted-asymmetry test pinning what each engine actually does") **was never available**: the M:N shape varied by thread count AND across runs at one thread count. (3) A one-native-call marker is the exact shape that hides instance (2) — when the contract is "the REST of the job runs", the fence needs a "rest". Accepted cost: graceful `shutdown()` only — a submitted job that never prints and never returns now hangs `\| head -1` (`shutdown_now()` still kills it in 54 ms; a loop back-edge IS a cancellation point). CPython hangs identically; Go exits via SIGPIPE on fd 1, a signal policy Chezzi does not adopt. Nurseries unaffected (they abort siblings on any fault, by design) |
| ~~**W7-18**~~ | `:5155` | **FIXED 2026-08-05.** `--timeout` now reaches a fiber parked on the NETPOLLER — **10001 ms hang (no verdict, no output, killed by an external `timeout 10`) → 304 ms `TIMED-OUT t`**, stable 10/10 at `CHEZZI_THREADS=1/2/3/4/8`, and the aborted task still runs its `defer`s *including a socket write inside them*. Go is the ancestor and agrees: `go test -timeout 300ms` against a goroutine on `net.Listener.Accept()` panics `test timed out after 300ms` and never runs the following `t.Fatal`. A park now registers for `min(the op's own D6c timeout_ms, the run deadline)` and the resumed op **re-reads the clock** to tell the two apart | **The filed premise was WRONG, and it is the whole lesson.** The row said the fix "needs a second marker distinct from `poll_timed_out`, threaded through the 5 `PollPark` construction sites plus the re-inject." It needed none: `Vm::deadline` is ALREADY an absolute `Instant` on every worker and `Some` only under `--timeout`, so `now >= self.deadline` at resume answers exactly what the marker would have carried. `PollPark`, `poller::register`, `next_timeout` and `fire_due_socket_timeouts` are untouched. Second lesson: the obvious spelling of that insight re-introduced W7-16's skipped-`defer` bug on **three** separate paths (halt-before-take, `?` past `demote_socket_exit`, clear-only connect resume) and adversarial review found a **fourth** — a top-level `connect` handing the abort back as a *catchable* `Err`. All four shipped green |
| ~~**W7-17**~~ | `:5081` | **FIXED 2026-08-05.** `--timeout` now reaches a timer wait PARKED in a `parallel:` nursery with no runnable sibling — **3004 ms `FAIL … SWALLOWED` → 304 ms `TIMED-OUT t`**, for BOTH park sites (the single `timer(ms).recv()` and a `wait:` timer arm), at `--threads=1/2/3/4/8`/default, and the aborted task still runs its `defer`s. A third site fell out of the same measurement: the **serial cooperative `wait:` timer arm** was still a bare `thread::sleep` — the one inline-sleep W7-16 missed — also 3004 ms → aborted. The park's timer job now fires at `min(its own deadline, the run deadline)` and delivers `true` ONLY if its own deadline really passed — an early wake just requeues the fiber (`close_wake`) — and `chan_recv_step`/`op_wait_poll` gained a `--timeout` checkpoint so the re-check turns that wake into the hard abort. Go is the ancestor and agrees: `go test -timeout 300ms` against `<-time.After(3s)` panics at 300 ms and never runs the following `t.Fatal` | **The filed lesson was WRONG, and it is the interesting part.** The row concluded "fixing it means giving parked fibers a deadline-driven wake (a scheduler feature), not another checkpoint: chunk-re-arming the park's timer job only gets a wake, and the resumed fiber would re-park because its own deadline has not passed." Both halves were true in isolation and the conclusion did not follow: **a wake and a checkpoint are the fix, together** — neither alone is anything. The re-park it predicted is exactly what the missing checkpoint prevents, and no re-arming was needed at all (one clamped wake, so W7-16's 200-re-arms/s cost is not paid here). The scheduler-level alternative it pointed at is also *worse*: `flag_deadlock` drops parked fibers without `unwind_deferred`, so it would have re-introduced W7-16's skipped-`defer` bug, while wake-and-re-check faults from inside the VM and unwinds normally (fenced: `a_timer_parked_task_aborted_by_the_deadline_still_runs_its_defers`). Second lesson: the **runnable sibling in every neighbouring fixture is what hid this for a whole milestone** — a spinner's own back-edge trips the deadline at 303 ms, so W7-16's tests passed over it; the new fixtures deliberately have no sibling |
| ~~**W7-20**~~ | `:5737` | **CLOSED 2026-08-05 — NOT A BUG, documented.** FFI bytes reach fd 1 without passing any VM sink, so the broken-pipe halt cannot see them: an `extern "libc.so.6": fn puts` loop under `\| head -1` spins (6002 ms to an external kill, rc=0, no fault) where the same loop using `print` exits in 3 ms. **Both owning ancestors do the identical thing** — CPython `ctypes` and Go cgo each spin (6001 ms, killed) while their native prints die in 37 ms / 2 ms — and the `print`-vs-FFI output ordering is byte-identical to CPython's, `io.flush()` included. Documented in `syntax.md` §12b + two `stdlib.md` cross-refs | **The filing's own ranking was backwards, which is the lesson.** It offered "flag the fd-1 writers at `extern`" vs "leave it and document", calling the second *"cheaper and narrower"* — the budget option. Measuring inverted it: the flagger is not better-but-pricier, it is **wrong**, since it would drift Chezzi away from both ancestors on a surface where it currently matches them exactly (and any C function can wrap `puts`, so the symbol list is incomplete by construction). "Cheaper" was doing the arguing; nobody had run `ctypes` yet. Also found, after two prosecutors ran the shipped snippet: the doc's first draft blamed glibc for swallowing the error (*"`puts` + `fflush(NULL)` never reports"*) when the fault was its own **wrong-width extern** — bare `int` is C `long`, so `puts`'s C-`int` `-1` arrives as `4294967295` and the guard silently dies; declared `int32` it detects at i=1638, exactly where CPython's `ctypes` does. "The library does not report X" is a claim about someone else's code made from one local observation |
| ~~**W7-19**~~ | `:5679` | **FIXED 2026-08-05.** `fs.stat`/`fs.walk` were the only two of `std.fs`'s seventeen members outside the blocking set, so their syscalls ran inline and PINNED an M:N core worker — `walk` for a whole tree walk. Both are `Kind::Blocking` now. Measured at `CHEZZI_THREADS=1` on a 121k-entry tree, paired against the same commit with only these two entries reverted: a sibling fiber's worst scheduling gap **136–139 ms → 38–41 ms**, 4 concurrent `fs.walk`s **814–825 ms → 449–469 ms**. Go and CPython both hand the worker off here (P released on a blocking syscall; GIL dropped around `os.stat`/`os.walk`) | The off-heap-safety proof the row demanded was already satisfied by precedent: both take their path via `Host::arg_bytes`, and neither return shape is new to the boundary — `_list_dir` already offloads the same `Ok(List([Bytes…]))` and `process.run`/`run_args` the same `Ok(Struct{…})`. **The residual is instructive**: ~39 ms, not ~5 ms, because *result lowering* (121k paths → heap objects) needs the `Vm` and so stays on the core worker — offloading moves the syscall, never the allocation |
| ~~**W7-5e**~~ | `:5624` | **FIXED 2026-08-05.** The W7-5d halt gate (`Vm::stdout_writes`) assumed every streamed stdout write goes through `Vm::emit_out_bytes` — true, enforced by nothing, and a new native reaching `stream::write_out` another way would silently lose its halt (`\| head -1` spins on a loop calling it). `write_out` now **takes the writing `&mut Vm` and bumps the counter itself**, so counting and emitting are one statement and a bypass does not compile (`error[E0061]`, verified by writing it). Still per-`Vm`. Zero behavior change: `\| head -1` on a 100 000-line print loop exits at **4 ms, rc=1, `stdout closed (broken pipe)`** at default M:N and `--threads=1/2/4`; all 53 `tests/interactive.rs` fences green | **This row's own reasoning is the lesson.** It ruled out the *direction* — "cannot be enforced by moving the counter into `stream::write_out` — that would make it PROCESS-global" — when only the `static`-beside-`OUT` **spelling** is global; a move that carries the `Vm` is not. The three fences it ranked instead all work around `write_out`, and the one it called the real fix (make it private to `exec.rs`) **is not expressible at this file layout** — Rust has no friend visibility and `pub(in path)` names only an ANCESTOR, never a sibling (only re-parenting the file under `exec/` would say it). Generalizes: when a filing rejects a direction, check it rejected the direction and not one spelling of it — everything ranked below inherits the error. Scope, from adversarial review: this closes the VM's own sink, not fd 1 — FFI still writes it uncounted, filed as **W7-20** and closed there as not-a-bug (both ancestors do the same) |
| ~~**W7-16**~~ | `:4939` | **FIXED 2026-08-05.** A wait whose **deadline WE own** (`time.sleep_ms`, `timer(ms).recv()`) is now a **CONTINUOUS** cancellation + `--timeout` checkpoint (~5 ms), everywhere: nursery, eager `Executor`, top-level `main`, both engines. `shutdown_now()` at 50 ms against `sleep_ms(3000)`: **3005 ms → 55 ms**, and the post-sleep code no longer runs; timer form 3005 → 55 ms; nursery mid-flight 3005 → 55 ms. A syscall-blocking native (`fs.*`/`request*`/`process*`/`io.*`) stays deliberately ENTRY-only — a `read(2)` in the kernel is not ours to cut short | **The filed premise was WRONG in two ways, and measuring it is what found the real bug.** (1) *"The same `sleep_ms` inside a nursery is interrupted on both engines"* — **no**: the parity fence only passed because its `boom()` faulted BEFORE `napper` entered the sleep. Move the fault 50 ms later and the nursery ran the full **3005 ms M:N / 3054 ms serial** and printed `napper woke`. There was no nursery-vs-executor split to reconcile; both were broken mid-flight. (2) *"`--timeout` cannot reach these jobs"* is not executor-specific — `chezzi test --timeout=200` reached **no timer wait anywhere**: top-level, nursery AND executor sleeps all ran their full 3 s and reported **PASS** (only a busy `while` loop bucketed TIMED-OUT). A documented "Hard-abort" guard that silently never fires. (3) The contract question resolved AGAINST the CPython pairing the row proposed: `ThreadPoolExecutor.shutdown(cancel_futures=True)` (3001 ms, no interrupt) is the *thread*-blocking sleep; Chezzi's is a FIBER wait, whose ancestor is `asyncio.sleep` under a `TaskGroup` (cancelled @50 ms) / Go's `select { <-time.After; <-ctx.Done() }` (@100 ms) — both cancel. Decisive internal evidence: an eager job blocked on a plain `ch.recv()` **already** died at `shutdown_now()` in 56 ms, so exempting sleep was an internal inconsistency, not CPython fidelity |
| ~~**W7-14**~~ | `:4974` | **FIXED 2026-08-04.** The cooperative inline-sleep is now gated off for every waiter that owns its OS thread — an eager `Executor` job, the top-level `main` thread, and `main` inside a native callback — each of which blocks with the timer as one more arm and clamps its in-place wait to the deadline. `timer(300)` beside a value at 50 ms: **`timer` @ 306–308 ms → `value 9` @ 56–57 ms** on all three, matching the nursery path (54 ms) and Go's `select` | Three lessons. (1) The blocker on file — "WAIT-1's recipe does not port: it submits the background deadline send into `self.mn`, which an eager job does not have" — was solving the wrong problem. WAIT-1 injects a wake because a PARKED FIBER has no thread; a party that owns its thread needs no wake at all, only a shorter timeout. (2) The dead clamp W7-13r(a) deleted was not dead code but a SYMPTOM: it was unreachable *because* of this bug, and reading it as "provably `None`" documented the bug as an invariant. (3) **The first fix reused `can_block_in_place()` and shipped green with a third path still broken** — that predicate folds in `is_counted_party`, i.e. `native_reentry == 0`, which is a rule about being JUDGED, not about being able to block. Adversarial review caught it; the tests did not, because they only covered the two paths the fix was written for |
| ~~**W7-13r**~~ | `:4619` | **ALL THREE RESIDUALS FIXED 2026-08-04**, with W7-13 itself: (a) the eager `wait:` arm was a blind `thread::sleep` — now waits on arm 0's condvar, 300 wakeups **1020 ms → 5 ms**; (b) `trip()` set `done_latch` outside `core.q` — now under it; (c) a blocked eager `send` never observed `close()` — was a HANG, now faults `send on a closed channel` at 105 ms (Go, compiled: 104 ms) | Kept for the lessons, not the status. (a) was deferred as "needs its own design primitive" — **wrong**, `demote_wait_block` (`sched.rs:1114`) already had the four-line trick. (b) is deliberately UNFENCED: the window is nanoseconds and measured 5–6 ms both ways, so a timing test would assert nothing. (c) left a DELIBERATE engine divergence (`--serial` keeps `FULL_SEND_DEADLOCK`: its drain runs jobs one at a time and cannot interleave them) and needs ≥2 pool threads, unchanged by the fix. Note the original W7-13 diagnosis (a *missing* `wake_senders`) was WRONG: that wake already fires on all six pop paths; the bug was a LOST wakeup — `eager_wait_tick` waited with no predicate, so a `notify_all` arriving while the lock was free hit a condvar nobody was on yet |
| ~~**W7-15**~~ | `:5142` | **FIXED 2026-08-04**, found while measuring `W7-12r` and previously unfiled. `main` blocking on a channel an eager `Executor` job was about to fill FAULTED `recv on an empty channel: deadlock` where Go and CPython both print the value — a WRONG ANSWER, not a hang, on a three-line program. Cause: `chan_recv_step`'s "I have no scheduler ⇒ nobody can ever send" `else` arm, the stale premise `future.md` §2d names, which stopped being true when eager execution put running jobs outside every scheduler. `main` now blocks like any other counted party | — |
| ~~**W7-12r**~~ | `:4442` | **FIXED 2026-08-04** by the process-wide quiescence detector (`src/vm/quiesce.rs`, `future.md` §2d **step 0**), which DELETED W7-12's per-executor predicate whole. All three residuals close — (a) two blocked jobs in one executor, (b) two executors deadlocking each other, and (c) a blocked job with no explicit `shutdown()` all fault in <10 ms where they hung forever. The same change fixed an unfiled WRONG ANSWER found while measuring them (`W7-15`, below): `main` blocking on a channel an eager job was about to fill used to fault. Kept for the lessons | Three, all about *when* a verdict may be formed. (1) The verdict must be ONE observation — a first cut cloned the party list and released the lock before reading channels, and reported a producer and consumer parked on channels that were never both empty at any single instant. (2) A party must not stay registered across its own retry: `pop()` and un-registering are not atomic, so it reads as parked at the instant it made progress (both caught by the existing 300-handoff `wait:` fence, not by reasoning). (3) SATISFIABILITY ("is this wait already over?") is what replaced the debounce — a direct question, where "has nothing moved recently?" was a guess that faulted a healthy cap-1 pipeline 6/40 runs |
| ~~**W7-11**~~ | `:4771` | **FIXED 2026-08-04** by the same-guard whole-container fallback (`Vm::from_wire_piece`), plus `at(i) -> Option[E]`. A dangling `Backref` no longer `.expect`s: it flags, and the view re-rebuilds the whole container and returns the piece by its wire id, so the cycle survives — CPython's measured answer. Kept for the two lessons: the W7-4 round-2 rejection of "rebuild the whole container" did **not** transfer (it fired on EVERY piece and re-read the box under a SECOND guard; this fires only on a dangling backref and borrows the caller's guard), and the `.expect` was reachable from a legal single-threaded program for months because no test ran a cyclic value THROUGH a copy-out view | — |
| ~~**W7-4a**~~ | `:3901` | **FIXED 2026-08-05.** Cell identity was per MODULE in the snapshot, so two globals in DIFFERENT modules over one shared cell arrived as two cells and a task's write through one was invisible to the other: `0`, where CPython (`import pk, pl` + a thread) and Go (two packages + a goroutine) both **measure `2`**. Fixed by one `WireMemo` spanning the whole snapshot (`emitted` cleared per module so each stays self-contained under lazy fault order) + one `Vm`-lived rebuild map (`Vm::snapshot_rebuild`), rooted and swapped with the view | Kept for the lesson: the ceiling comment predicted this would need rooted state "across GC-visible points", and the rooting turned out to be **belt-and-braces** — the test still passes with the `collect` root line deleted, because every entry is also reachable from the global it was just `module_define`d into. Predicted cost ≠ measured cost |
| ~~**W7-4b**~~ | `:3901` | **FIXED 2026-08-05.** A cell reaching the `SnapValue::Cell` slow arm carried no id, so its binding split: `p := [k]` (a captured local holding a module) read `1` where CPython **measures `3`**. Fixed by giving `SnapValue` the same `Cell { id, inner }` / `Backref(id)` encoding the wire arms have, minted from the SAME memo and drained by the same rebuild map | The filed premise was **stale**: `Native`/`Cffi` cross BY VALUE now, so they never force the slow arm — only `Obj::Module` does (`has_handle` = `WireValue::Handle` = `Obj::Module` alone). The code called that "source-unreachable, defensive only", but a module IS bindable to a local (`m := k`), so a cell over `[k]` reaches it. **Re-derive a residual's premise before pricing it**; the "snapshot FORMAT change, out of proportion" estimate was for a residual that had drifted |
| ~~**W7-4c**~~ | `:3901` | **FIXED 2026-08-06.** ONE TASK reached through TWO serializations now gets ONE binding: a `spawn:` block's captures and the module-global snapshot rebuild the same cell. `0` → **`2`**, matching CPython. The snapshot is pinned BEFORE the spawn-time clone, the clone carries its ids forward on the task (`QueuedTask::cell_ids`), and `rebuild_ready` + `fault_module` drain ONE map | Cost: **+10.2% on a 120k-spawn storm** that gains nothing from it (filed below as an open perf item); snapshot stress flat (+0.2%). Adversarial review found TWO criticals first — see §W7-4c |
| ~~**W7-4d**~~ | `:3901` | **CLOSED 2026-08-05 as NOT-A-BUG (resolved by design).** An `RwShared` COPY-OUT VIEW (`at`/`for_each`/`fold`/`get_key`/`has`/`for_each_entry`/`fold_entries`) rebuilds one piece per step, so two sibling closures pulled out separately do not share their binding | Inherent to a copy-out API — two `at()` calls ARE two crossings, and identity is per crossing by definition. A whole `get()`/`read()`, and `slice` (one call returning a container), are one crossing and DO share. Never a residual of the fix; "fixing" it would reopen the round-2 O(n²) + concurrent-`set` hazard. Documented in `concurrency.md` §airlock |

**Known limits that are documented, not bugs** (listed so they aren't re-filed): `Iterable[T]` element
recovery does not fire for a struct with only `iter` and no `next` **in BOUND position** — bound that
one concretely (`[S: Iterable[int]]`) or annotate the parameter `Iterable[int]`, where the annotation IS
the element type and nothing has to be recovered (`syntax.md`); read-only covariance is deliberately NOT
part of the model, so `List[int]` → `Iterable[Any]` (and `Iterable[int]` → `Iterable[Any]`) is REJECTED —
a protocol existential is strictly invariant in its args, same as `List`/`Map`/a user generic struct;
`.compare()` answers the operator's verdict wherever the operator has one and only falls
back to `sort()`'s total order for NaN, so a `±0.0` pair compares Equal by the method while `sort()`
orders `-0.0 < +0.0` (`spec.md`); `--max-heap`/`--timeout` are M:N-only by design.

## Checker / type system

### Generic methods on RESERVED built-in receiver types — turbofish + bodied-method inference (found 2026-07-22, **1a/1b/2 RESOLVED 2026-07-22**)

A *generic* method call (`recv.m[U](...)` or inference of `U`) works on a **user struct** receiver
(`Ty::Struct`) but was **broken when the receiver is a reserved built-in type** — `Ty::List`/`Map`/`Set`
/`Shared`/`RwShared`/`Atomic`/`Executor`/`Socket`/`Listener`/`Writer`/`Reader`. Three facets, all fixed:

- **FIX 1a — turbofish on reserved receivers (RESOLVED).** `method_has_own_type_params`
  (`src/checker/expr.rs`) gained reserved-receiver arms (look up `self.structs.get(bare)` like the
  `Ty::Struct` arm — the container/concurrency method tables are re-seeded there under bare names), so
  the member turbofish `[1,2,3].map[int](…)` and any bodied generic method are no longer rejected
  *"method 'map' takes no type argument(s)"*. Non-generic methods (`filter`) still correctly reject.
- **FIX 1b — bodied generic method inference on the 4 concurrency handles (RESOLVED).** The
  `Ty::Shared`/`RwShared`/`Atomic`/`Executor` arms now route a harvested method whose sig carries
  `type_params` through `infer_generic_method` (prepend the concrete receiver, since the harvest strips
  `self`) — verbatim mirror of the `Ty::List` arm. So a bodied `fn m[U](self,f:fn()->U)->U` opens `[U]`
  and infers `U` from the closure, instead of failing *"expected fn()->U, found fn()->int"*.
- **FIX 2 — bodied-method runtime dispatch on the 4 concurrency handles (RESOLVED).**
  `try_native_bodied_method` is now called from the `Shared`/`RwShared`/`Atomic`/`Executor` arms of
  `do_method_call` (`src/vm/call.rs`), mirroring the `Writer`/`Reader` arms (try-bodied BEFORE native; a
  miss falls through byte-identically). Closes the check-OK/run-fault gap for bodied methods on those
  handles. **(2026-07-22 hardening, `auto-task/unify-native-dispatch-prefix`)** the eight per-handle
  arms were then folded into ONE `match self.heap.get(h)` key-map in `do_method_call`, and the checker's
  reserved-handle arms into `resolve_native_handle_method` — so bodied dispatch can no longer be
  forgotten on a NEW handle (adding it to the one match auto-enables it). Behavior-preserving; the fold
  also drops the eight `if matches!` probes off the hot list/map/struct method path.

**Shipped proof:** `Executor.submit_result[T](self, f: fn() -> T) -> Channel[T]` (`std/concurrency.chz`)
— the FIRST bodied generic method on a native struct, exercising 1a/1b/2 end-to-end. `submit_task`
(`std/concurrency/task.chz`) now builds over it. Tested both engines
(`vm::tests::executor_submit_result_both_engines`, checker tests
`reserved_receiver_generic_method_turbofish_ok` / `executor_bodied_generic_method_infers_from_closure`).

**Residual (deliberately NOT done):**
- **list/map/set bodied methods stay unharvested by design.** FIX 1b/2 cover only the 4 concurrency
  handles; the hot `list`/`map`/`set` `core_method` arm (`src/vm/call.rs`) is deliberately left untouched
  (an extra per-call table probe on `list.push` in loops = M19 perf risk, and no bodied methods exist
  there). List/Map/Set still reject bodied methods at check, so no check-OK/run-fault divergence.
- **`ex.submit_task(f)` dot-form** (a bodied generic method returning a *user* `Task[T]`) still needs the
  deferred Task-placement/harvest change (Option A — move `Task` so `Executor` can name it as a return
  type without an import cycle). `submit_task(ex, f)` free-fn form remains the shipped API.

## NEXT-SESSION BACKLOG — sendability completeness (deferred from 2026-07-21)

Three items to make the airlock sendability model Go-consistent (Go sends interfaces + closures over
channels; Chezzi should too). All DEFERRED to their OWN future sessions — do NOT bundle. Each is its own
spec. Ranked by value ÷ risk. (Context: Task 1 "align serial" landed 2026-07-21, `serial == M:N` by
construction; these three finish the sendability story.)

### 1. Protocol sendable under **(a)** — "Task 2" — **DONE 2026-07-21**
**LANDED.** All user protocol existentials are now sendable (Go `chan interface` parity): `Channel[P]`,
protocol-typed spawn args / struct fields / `Ok`/`Err` payloads / returns all type-check. The change was
**one logic line**, not a widening-site sweep: `sendable_rec`'s `Ty::Protocol` arm → `true` (was the
hardcoded `sendable_bounded(p) == "Error"`, now deleted), and `assignable`'s Protocol arm keeps the
existing `&& self.sendable(a)` concrete-witness guard uniformly.
**Premise correction (the old note above was wrong on two points):** (1) `assignable`
(`src/checker/proto.rs`) is the SOLE concrete→Protocol widening chokepoint — every widening category
routes through it, so there was **no widening-site coverage risk** and no sweep to do. (2) The
"non-sendable floor is FFI/handles" framing was backwards: the CHECKER marks FFI/`Func`/handles
**sendable** (`Ty::Func`/handle types are sendable) — the RUNTIME airlock (`ensure_crossable` over
`has_handle`, `src/vm/{sched,wire}.rs`) is the real gate for a genuinely-unserializable witness (one
carrying an FFI handle, a mid-`recover:` generator), rejecting it recoverably and identically on
serial == M:N. Post-change `sendable_rec` returns `false` only for `Ty::Module` (near-unconstructible as
a value), so the witness-sendable clause is near-vacuous — protocols behave like every other type.
Genuine-rejection coverage moved to the runtime: `vm::parity_tests::ffi_handle_cannot_cross_airlock_three_engine`.
Decision record: `~/.claude/plans/2026-07-21-task2-protocol-sendable-decision.md`.

### 2. Recursive-local-fn sendability — **DONE (2026-07-21)**
A nested recursive `fn` (and a mutually-recursive closure pair) now CROSSES the airlock and computes
correctly on both engines — the reject diagnostic is gone. Implemented via identity-preserving airlock
serialization on the `Obj::Cell` + `Obj::Closure` arms: a new `WireValue::Backref(u32)` + an `id`
on the wire arms. (Item **A** below since GENERALIZED this to every container arm, so self-referential
DATA crosses too.) `to_wire_depth` threads a back-edge memo (`WireMemo` — a
`FxHashMap<GcRef,u32>` DFS-stack set + id counter); on a revisit of a Cell/Closure still on the serialize
stack it emits `Backref(id)` and stops. `from_wire` ties the knot: alloc a placeholder `Cell(Nil)`/
`Closure(captured=[Nil;n])` FIRST, register `id→GcRef`, recurse children, then `heap.get_mut`-patch —
memory-safe because `Heap::alloc` never collects (no GC between placeholder and patch) and `GcRef` is a
GC-traced index. The old `graph_reaches_handle` reject (both call sites + the fn) is deleted.

**Corrected premise (the pre-work brief was wrong):** there was NO pre-existing cycle-safe airlock
serializer to mirror — `WireValue`/`SnapValue` were owned Box/Vec TREES with no identity/placeholder arm,
and `examples/airlock_cycle.chz` REJECTED cycles (`maximum structural depth exceeded`), it never
round-tripped them. This is brand-new identity-preserving machinery. (Item **A** later extended it to
container arms, and `airlock_cycle.chz` now ROUND-TRIPS — see below.) **Design deviation from the literal
task spec (recorded):** the memo is BACK-EDGE-ONLY (pops a node off the stack on DFS exit), so only a TRUE
cycle earns a `Backref`; an acyclic DAG alias (e.g. one arg `[f, f]`) is re-serialized as an independent
deep copy — preserving the documented Cell/closure deep-copy-independence contract (`wire.rs` §F1). A
plain visited-set (the literal spec) would have SHARED such aliases, a silent regression. Byte-identical
to the spec on every genuine-cycle case (self-recursion, mutual recursion, recursive-closure-capturing-an-
outer-local). **(SUPERSEDED by item A:** originally Struct/List/Map/etc. earned NO id — a pure-data cycle
tripped the depth cap; item A gave every container arm an id + `Backref`, so self-referential data now
round-trips too and `airlock_cycle.chz` FLIPPED to crossing.) Tests: `airlock_recursive_local_fn_round_trips_
both_engines` + `_under_gc_stress`, `airlock_mutually_recursive_pair_round_trips`, `airlock_recursive_
closure_captures_outer_local_round_trips`, `airlock_aliased_closure_stays_independent`, `generator_
carrying_recursive_closure_round_trips_both` (and `generator_parked_slot_nonsendable_rejects_both`
repointed by item A to a >10000-deep ACYCLIC parked slot, which stays a both-engines depth-cap reject).

### 3. Reject-case generators — mid-`recover:` (arm b) DONE; pending-`defer`/multi-frame (arms a,c) checker-unreachable
**Arm (b) — suspended mid-`recover:` — DONE (2026-07-21).** A generator suspended inside a `recover:`
block (a live handler stack in its parked context) now CROSSES the airlock and RESUMES with its recover
boundary intact. A `Handler` (`src/vm/mod.rs`) is pure plain-data (all `usize`, `Copy`, no `GcRef`/`Value`),
so it serializes as-is on `WireGenState::Suspended` (`src/vm/wire.rs`) with no value recursion; `from_wire`
rebuilds the frame/stack coherently so the handler indices stay valid. `generator_next` (`src/vm/exec.rs`)
rebases every parked frame's / handler's `nursery_len` to the resuming driver's floor at swap-in — a
generator provably opens no nursery of its own (`spawn`/`parallel:` are checker-banned inside a generator,
recover blocks included), so its escape-drain must be a no-op; this makes the stale cross-heap `nursery_len`
inert and also fixes a latent SAME-HEAP over-drain (resuming a mid-`recover:` generator at a deeper nursery
floor than it was first driven wrongly cancelled the driver's live sibling `spawn`s). Tests (`src/vm/
parity_tests.rs`): `generator_recover_suspended_resumes_both`, `generator_crossed_recover_catches_fault_
matches_control_both` (+ its inline `generator_recover_fault_control_inline` control — the item-#2 semantic
guard: the resumed recover must CATCH and produce the correct recovered value, matching a no-airlock
control), `generator_crossed_recover_fault_leaves_siblings_intact_serial` (the rebase, serial oracle).

**Arms (a) multi-frame + (c) pending-`defer` — NOT built; CHECKER-UNREACHABLE by construction; clean
reject KEPT as a defensive guard.** (a) `yield` only fires in a generator's own body frame (`in_generator`
resets at every fn/closure boundary), so a suspended generator always has exactly one frame — no
checker-valid source constructs a multi-frame suspension. (c) `defer` is banned inside a generator body
(`checker::sig`: "`defer` is not supported inside a generator", recover blocks recursed into), so a parked
frame can never carry a pending `defer`. The `to_wire` rejects (`src/vm/sched.rs`) stay as belt-and-braces
guards against the type-blind compiler path (the parity harness `run_program_inner` compiles WITHOUT the
checker); there is no coherent state to serialize, so nothing is built. Reject test kept:
`generator_parked_defer_rejects_clean_both`. Both engines reject IDENTICALLY (completeness, not a bug).

## NEXT-SESSION BACKLOG — sendability CONSISTENCY carve-outs (deferred from 2026-07-21)

After the 3 items above landed, the airlock is ~99% complete. Auditing "what's still unsendable" surfaced
that the genuinely-FUNDAMENTAL limit is only ONE thing — **a value carrying a live host handle**
(`Obj::Module`/`Obj::Native`/`Obj::Cffi` → `has_handle` → `ensure_crossable` "module/native/FFI handle
cannot cross"). A foreign OS/library resource cannot be memcpy'd into another heap; this is correct and
stays. (The concurrency handles `Channel`/`Shared`/`RwShared`/`Atomic`/`Executor`/`Socket`/`Listener`/
`Reader`/`Writer` cross as shared `Arc` cores; `ptr` crosses by value — none are in this set.)

The OTHER two remaining rejects are **arbitrary carve-outs**, not fundamental limits — the
identity-preserving `WireMemo`/`Backref` machinery built for recursive-fn sendability (item 2 above) can
close both, and doing so removes exactly the kind of "why can THIS cross but not THAT?" drift the no-drift
north-star forbids. Each is its OWN spec/session — do NOT bundle. Ranked by value ÷ risk.

### A. Self-referential DATA sendable — extend `Backref` to container arms — ✅ DONE (2026-07-21)
**LANDED.** Every container `WireValue` arm (`List`/`Tuple`/`Map`/`Set`/`Struct`/`Enum`/`NewType`/`Iter`)
now carries a per-serialization `id` + a `Backref` exactly like `Cell`/`Closure`, so a self-referential
struct/list/map (`a.next = b; b.next = a`) ROUND-TRIPS across the airlock (`spawn` arg / `Channel.send` /
`Shared` / module-global snapshot) instead of tripping `maximum structural depth exceeded`. `to_wire_depth`
inserts each container's GcRef into the `WireMemo` DFS stack BEFORE recursing (back-edge → `Backref(id)`,
removed on DFS exit so an off-stack DAG alias stays an independent deep copy); `from_wire_memo` ties the
knot in every container arm (placeholder-alloc → register `id` → recurse → `heap.get_mut`-patch). This was
a **net-deletion** change: the `WireMemo.nonpreserved_depth` machinery + BOTH mixed-cycle guards (commit
e8dcad7) are GONE — a mixed struct+closure cycle now just round-trips. **CORRECTION to the original spec
premise:** the note "`from_wire` already threads the `rebuild` map through every container arm, so the
tie-the-knot reconstruction is largely in place" was **WRONG** — the container arms recursed children
BEFORE alloc, so a nested `Backref` would have hit an unregistered id; the `from_wire` rewrite (every arm
placeholder-allocs + registers before recursing) was the bulk of the work. `examples/airlock_cycle.chz` +
its golden now ROUND-TRIP (sections 1-3); the depth cap STAYS as the backstop for genuinely-unbounded
ACYCLIC nesting (section 4 control + `generator_parked_slot_nonsendable_rejects_both`, re-pointed at a
>10000-deep acyclic parked slot). The **sole** remaining non-identity-preserved container is `Generator`
(its parked frame holds no `WireValue` id, so it can't back-reference) — a cycle threaded through a
generator is caught by the `WireMemo.gens_on_stack` guard (re-entering the same generator on the
serialize DFS stack → clean `a generator cannot be sent across tasks as part of a reference cycle`
reject, NOT a silent duplicate: once the containers back-reference, the container back-edge cuts the
recursion before the depth cap would trip, so the generator arm must guard the cycle itself). Tests:
`airlock_self_ref_{struct,list,map}_round_trips_both`, `airlock_mixed_struct_closure_cycle_round_trips_both`,
`airlock_struct_dag_alias_stays_independent` (adversarial parity-blind independence),
`airlock_self_ref_struct_round_trips_under_gc_stress`, `airlock_cyclic_module_global_crosses_mn`,
`generator_in_data_cycle_rejects_both` + `suspended_generator_in_data_cycle_rejects_both` (the
gen+container cycle reject). `src/vm/{wire.rs,sched.rs,fxhash.rs,core.rs,stmt.rs}`.

### B. Module-GLOBAL live generator sendable by value — ✅ DONE (2026-07-21)
**LANDED.** A module-global live generator now crosses the airlock BY VALUE (deep copy), exactly like a
frame-local one (F3 path C) — the reach-gate + Option-B poison→`nil` model is RETIRED. `to_snap_depth`'s
fast path no longer excludes generator-embedding values (`!value_embeds_generator` clause dropped), so a
handle-free module-global generator with all-sendable parked slots rides the `SnapValue::Wire(to_wire…)`
lane. Its slow `Obj::Generator` arm, however, must **NOT** re-raise the `to_wire` reject: `snapshot_modules`
walks EVERY module global of a nursery's snapshot, reached or not, so eager-faulting there aborts any
program that merely *holds* a non-sendable module-global generator it never sends (a regression vs the old
poison→`nil`-then-reach-gate model). Instead the slow arm snapshots a non-sendable generator (non-sendable
parked slot / reference cycle / parked host handle) as an inert **`Nil` placeholder** — the untouched-global
program runs CLEAN, and a task that REACHES it faults recoverably at the use site (`cannot iterate over nil`),
byte-identical serial == M:N. (Fault only when reached — the "when reached" contract; the frame-local F3
path-C crossing still rejects eagerly at `to_wire` because it only crosses the value actually sent.)
Each task already snapshots every module global per-task (`ensure_snapshot`, both engines since `6dca22c`),
so two tasks reaching the same SENDABLE module-global generator each drive their OWN independent copy —
memory-safe because `from_wire` rebuilds a fresh `GeneratorCore` on the worker heap (no shared cross-heap
`GcRef`); a non-sendable one is inert `Nil` on every worker, so no cross-heap handle can escape either.
**Net-deletion:** the whole reach-gate machinery is gone — `check_task_generator_reach`,
`check_outer_pending_generator_reach`, `check_task_reach_conservative`, `scan_proto_reaches_generator`,
`proto_reaches_generator(+_rec)` and its resolve/scan helpers, `any_hook_reaches_generator`,
`any_module_global_embeds_generator`, `module_slot_embeds_generator`, `value_embeds_generator`, the
`gate_executor_queue` executor path, and the `has_generators` VM field. (The `SnapValue::Poison` variant is
gone too; the inert placeholder reuses `SnapValue::Wire(WireValue::Nil)`.)
**CORRECTION to the original spec premise:** the "serial=shared-ref vs M:N=by-value-copy divergence" that
rated this MED-HIGH (why the `value_embeds` clause + `Poison` were kept, commit `7b73e7c`) was STALE after
`6dca22c` — the serial engine ALSO snapshots module globals per-task via the same memoized
`ensure_snapshot`/`to_snap`, so a per-task by-value generator copy is `serial == M:N` by construction.
Tests: `generator_module_global_{reached_crosses,suspended_reached_resumes,two_tasks_independent_copies,
parked_slot_nonsendable_rejects,in_data_cycle_rejects,unreached_nonsendable_runs_clean,via_executor_crosses}_both`
+ `generator_cross_module_member_call_crosses_both` (`src/vm/parity_tests.rs`). The memories
`generator-airlock-option-b-reach-gate` + `airlock-sendability-architecture` describe the retired model.

### NOT on the backlog (settled — not limitations)
- **Module handles** (a `Module`'s mutable globals in a value) — fundamental, stays rejected. Correct.
  Also source-unreachable (`module` is not a nameable type), so it's a defensive-only runtime guard.
  (Native `Obj::Native` + FFI `Obj::Cffi` fn values are NO LONGER here — they are pure code and now
  cross the airlock BY VALUE / shared `Arc`, exactly like a builtin fn; see the 2026-07-23 session log.)
- **Multi-frame / pending-`defer` suspended generators** — checker-UNREACHABLE (item 3 arms a/c); no valid
  program constructs them. The rejects are defensive guards, not a user-visible limit — nothing to build.

## Session log — 2026-07-28 (bug-hunt wave 7 — batch A: 3 host-boundary findings, ALL FIXED, no open rows)

Three defects at the **host boundary** (the native/CLI seam where raw OS bytes become Chezzi values) —
one P0 data-loss, two P1 host-panics. All three are fixed in this batch; **batch A adds NO row to the
OPEN ITEMS table**. Batch A deliberately does NOT fix the separately-filed **lossy path DECODE**
(`fs.list_dir`/`walk`/`glob`/`canonicalize`, `os.getcwd` decode a directory entry lossily and hand back a
path that does not exist) — it is uncoupled from these three and was its own later task, filed as
**W7-8** and **FIXED 2026-07-31**.

- **W7-1 (P0, DATA LOSS) — `fs.copy(p, p)` truncated the file to 0 bytes and returned `Ok(nil)`. FIXED.**
  `std::fs::copy` opens the DESTINATION `O_TRUNC`, so a self-copy wiped the file and reported success —
  check-OK, run-OK, data gone, byte-identical on both engines (parity-blind, like most of wave 6). It
  also fired when the two paths reached one inode through a **symlink**, so a path-string compare is not
  a fix. `copy` (`src/native/fs.rs`) now guards on **inode identity** (`dev`+`ino` via
  `MetadataExt`, `canonicalize` on non-unix) BEFORE the copy and returns a recoverable
  `Err("{from} -> {to}: are the same file")`, leaving the bytes untouched — matching Python
  `shutil.copyfile`'s `SameFileError` and coreutils `cp a a`. A missing destination is never "the same
  file", so copy-to-a-new-path is unchanged. `rename` needs no guard (POSIX `rename(p,p)` is a no-op);
  `copy` is the only truncating *pair* in the tree. Pinned by `tests/chz/stdlib/fs_copy_test.chz`
  (4 tests: same-path, symlink, plus two controls) on both engines.
- **W7-6 (P1) — a non-UTF-8 CLI argument or script path host-panicked the CLI, rc=101. FIXED.**
  `std::env::args()` PANICS on a non-UTF-8 item, so `chezzi run hello.chz "$(printf 'A\xffB')"` aborted
  at `library/std/src/env.rs:876` **before the program started**, on both engines, regardless of imports
  — a HOST panic, so `recover:` could not see it. `src/main.rs` now uses `args_os()` + a lossy decode.
- **W7-7 (P1) — a non-UTF-8 environment variable host-panicked at startup, rc=101. FIXED.**
  Same shape one layer down: `HostConfig::from_process` snapshots the whole environment with
  `std::env::vars()`, which panics at `env.rs:162` on one hostile variable — killing even a
  `print("hi")` program that never touches `std.os`. `src/native/mod.rs` now uses `vars_os()` + a lossy
  per-key/value decode. `os.environ`'s **sorted-by-key** lowering is downstream (`src/vm/mod.rs`) and was
  not touched; re-verified by running its existing golden.

**Decoding rule chosen (documented in `docs/stdlib.md`, not silent):** argv and env reach Chezzi as
`str`, so they are decoded **lossily** (invalid byte → `U+FFFD`; two raw env keys can collide, last
wins). The bar this batch sets is "the CLI never host-panics on hostile bytes", not byte-fidelity.
**v1 ceiling, stated:** a script whose PATH is not valid UTF-8 still cannot be RUN — it now fails
cleanly (`cannot read '…'`, rc=1) instead of rc=101. Threading a real `OsString`/`PathBuf` through
would change `read_source`/`type_check`/module-graph-root signatures (resolver + checker), out of scope
for a host-boundary batch.

**Same meta-finding as wave 6 (an N-way set, fixed on only some arms):** the two panicking calls had
**three** siblings, not two — `src/bin/difffuzz.rs` and `src/bin/panicfuzz.rs` carried the identical
`std::env::args()`. Swapped too (dev-only drivers, no test); `grep -rn 'std::env::args()' src/` and the
`vars()` equivalent now return zero live call sites, which is the guard.

## Session log — 2026-07-25 (bug-hunt wave 6: 19 findings — W6-19 found while FIXING W6-2 — W6-1..W6-19 all FIXED as of 2026-07-27, the last being W6-9; of the 3 carve-outs filed (W6-3b/c/d), **W6-3b and W6-3c are FIXED (2026-07-26)** and **W6-3d was RESOLVED 2026-07-27 by ruling (a)**; a follow-up to W6-9 — **W6-9b**, the capture-based
parity comparators still diffing a lossy decode — was found by adversarial review and FIXED 2026-07-28. So
wave 6 carries no open DEFECTS; what remains is the disclosed residual W6-9r (W6-10s and W6-10r both closed 2026-08-06), filed as
their own rows. See the OPEN ITEMS table at the top, which is the authority — 2 never-hunted surfaces swept)

Pre-freeze adversarial hunt, 5 disjoint parallel domains, weighted at the two surfaces the wave-5
residual named as never audited (**FFI**, **GC + `unsafe`**) plus the concurrency code that landed
*after* every prior wave (`RwShared` read-views `3156f76`/`cc07f77`/`3fedb34`/`04796a3`, `chezzi test`
flags+caps `109bfb6`..`5ccf7a0`). ~1200 probes. **Every repro below was re-verified by the main loop on
the real `target/release/chezzi`, both engines**, before filing; one subagent claim was dropped as a
false positive (an FFI `str`-param lifetime UAF via `putenv` — CPython ctypes is equally UB there, and
`store_str` is an existing documented deferral, so the differential was luck, not a contract).

**None of the original 18 is a serial≠M:N divergence** — every one is byte-identical on both engines, i.e.
the parity oracle is structurally blind to all of them. That is now the dominant shape of what's left.
(The one exception, **W6-19**, was found later, while FIXING W6-2 — not by the hunt: it needs a task whose
first module-global touch is a write, which no probe happened to write.)

### THE META-FINDING — 5 of the 6 P0s are ONE class: a fix applied to SOME arms of an N-way set
This is the same completeness/partial-coverage class the 2026-07-23 sweep found 3 instances of. It is
still the highest-yield lever in the repo and it is cheap: **enumerate the arms, assert each one.**
- W6-3: `compare`/`str` intercepted on scalar receivers, the other ~9 intrinsic protocol grants not. **FIXED.**
  (Its NaN carve-out W6-3c is **FIXED (2026-07-26)** too: `.compare()` now answers the same total order
  `sort()`/`.min()`/`.max()` use instead of faulting — ONE order, one divergence, no fault.)
- W6-4: R1 swept `Socket`/`io`/`request`/`crypto` off `from_utf8_lossy`, missed `std.process`. **FIXED.**
- W6-1: `flush_core`'s non-empty-buffer arm flushes the inner core, its empty-buffer arm doesn't. **FIXED.**
- W6-6: the extern-collision guard fires for bare-keyed enum variants, not module-keyed structs.
- W6-9: `write_bytes` is byte-exact on the `File` arm, lossy on the `Stdout`/`Stderr` arms. **FIXED.**

### W6-1. `Writer.close()`/`flush()` on a `buffered` writer SILENTLY DOES NOT PERSIST — durability contract broken — P0 — **FIXED (2026-07-25)**
```chezzi
import std.io
fn main():
    match io.create("min.txt"):
        Ok(w0):
            w := io.buffered(w0, 4)
            w.write("abcdefgh")                      # 8 bytes > cap 4 -> mid-write drain
            print("close =", str(w.close()))         # Ok(nil)
            print("file  =", str(io.read_file("min.txt")))   # Ok()   <- EMPTY
        Err(e): print(e.message())
main()
```
Both engines, rc=0. Persists correctly **iff the buffer never filled**: cap4/len3 → `Ok(abc)`; cap4/len4,
len5, len8, cap1/len2, cap0 → all empty after a *successful* `close()`. Same for `flush()`. The bytes only
reach the fd when the heap is dropped at process exit, so an in-program reader, a `process.cmd` child, or a
sibling process sees a truncated/empty file after `close()` returned `Ok`, and a SIGKILL/abort after
`close()` loses the data outright — the exact guarantee `flush`/`close` exist to provide.
Reference (Python owns buffered-file semantics; Go `bufio` identical):
`python3 -c "f=open('py.txt','wb',buffering=4); f.write(b'abcdefgh'); f.close()"` → file is `b'abcdefgh'`.
**Root cause** `src/vm/fileio.rs:88-96`: the `Backing::Buffered` arm of `flush_core` returns `None` when
`buf.is_empty()`, short-circuiting the `self.flush_core(&inner)` at `:101` that the non-empty path DOES run.
A mid-write drain (`write_to_core`, `:58-64`) pushes bytes into the inner `BufWriter<File>` **without
flushing it** and empties `buf`, so the later `close()` flushes nothing; `close()` drops only the outer
`Backing::Buffered` (the program still holds `w0`, so the inner `WriterCore` isn't dropped either).
**Docs contradicted:** `docs/stdlib.md:415` (`close` = "Flush + close the handle"), `:417` ("Forgetting
`flush`/`close` … loses the tail — Go's footgun. Mitigated…"), and `flush_core`'s OWN doc-comment
("`Buffered` → drain the in-VM buffer to the inner core, THEN flush the inner (so `buffered(create(f))` is
durable on disk)"). Fix is one-line-class: recurse into the inner core even when `buf` is empty.
**FIXED (2026-07-25).** `flush_core`'s `Backing::Buffered` arm now ALWAYS yields
`Some((inner, mem::take(buf)))`, and the recursion site guards the WRITE instead of the flush
(`if !drained.is_empty() { write_to_core(..) } flush_core(&inner)`) — an empty `write_to_core` on a
`Stdout`/`Stderr` inner would otherwise hand `emit_out("")` to the parity sink / stream queue. All four
`Backing` arms of BOTH fns were enumerated: `flush_core`'s `File`/`Stdout`/`Stderr` were already correct
(the new unconditional recursion reaches the std-stream arms and stays an honest no-op), and
`write_to_core`'s `File`/`Buffered` arms are correct as-is.
Two more siblings surfaced by the enumeration, both fixed with it:
* **`WriterCore::Drop` (`src/vm/core.rs`) was NOT benign** — its `Buffered` arm wrote the drained tail
  only when the inner was `Backing::File`, so a **nested** `buffered(buffered(create(p)))` chain dropped
  its tail on the floor forever (`docs/stdlib.md` promises a *file*-backed buffered writer drop-flushes,
  and a transitively file-backed chain is file-backed). The arm now handles all four inner backings:
  `File` → write+flush, `Buffered` → append to the inner's own buf (its `Drop` cascades one level down),
  `Stdout`/`Stderr`/`None` → the documented no-op. Rust test (drop timing isn't `assert`-able):
  `vm::core::tests::drop_flushes_a_nested_buffered_chain_to_the_file`.
* **the recursion made `WriteErr::Closed` reachable from a core BENEATH the receiver**, which
  `writer_method` renders receiver-relatively ("flush on a closed writer") — a lie when it is the inner
  handle that was closed, and `close()` masks `Closed`, so it would have reported success for a flush
  that persisted nothing. Both recursion sites now `map_err(from_inner)`: an inner `Closed` becomes
  `Io("the inner writer this buffer drains into is closed")` — right handle named, not maskable.
The `Stdout`/`Stderr` lossy `write_bytes` was W6-9, filed separately and **FIXED 2026-07-27** (the sink is `Vec<u8>` now).
Tests: `tests/chz/stdlib/io_writer_test.chz` (mid-write drain via flush + close, never-filled control,
at-cap, cap=1, a nested two-level chain, and the closed-inner `Err` on both `flush` and `write`),
serial==M:N. Docs: `docs/stdlib.md`'s `flush`/`close` rows state the full-chain guarantee at OBSERVER
level for a **file**-backed chain (an in-process `read_file`, a child, a sibling process), NOT `fsync`
durability — and explicitly do NOT claim it for a `buffered(stdout())` writer, whose drained bytes go to
the same never-awaited background stdout queue as `print` and through the same (now byte-typed — W6-9)
sink, so `Ok` there means *queued*, not *written*.

### W6-2. A module global FIRST INITIALIZED AFTER the first nursery reads as `nil` inside later tasks — check-OK-then-run-fault + silently-wrong — P0 — **FIXED (2026-07-25)**
```chezzi
import std.concurrency
tot := AtomicInt(0)
parallel:
    spawn: tot.add(1)
n: int = 42
parallel:
    spawn: print("task sees n =", n)   # -> nil        rc=0 (!)
    # spawn: print(n + 1)              # -> runtime error: cannot apply Add to nil and int   rc=1
print("parent sees n =", n)            # -> 42
```
Byte-identical both engines. Three fault shapes confirmed: `n + 1` → `cannot apply Add to nil and int`;
`q.len()` → `type nil has no method 'len'`; `p.x` → `cannot read field 'x' of nil`. Reproduces with
`parallel:`/`spawn` and with a second `Executor`; does NOT reproduce when every global is declared before
the first nursery, nor with a second `submit` on the same executor.
**Root cause** `src/vm/sched.rs:3483-3499`: `ensure_snapshot` memoizes the `ModuleSnapshot` forever
(`snapshot_memo` is invalidated nowhere) and every later nursery/worker replays that frozen `Arc`
(`sched.rs:268-279`). A global whose `:=`/`=` had not yet executed when the memo was built is snapshotted
as an absent slot and replays as `Value::nil()`.
**Why this is NOT the documented limit.** `docs/concurrency.md:94` documents *staleness* — "a mutation by
ordinary sequential code between two nurseries … is NOT seen by tasks that read the global afterward" — and
that behavior is correct and verified (`n: int = 1` then `n = 42` → task sees `1`, a legal `int`). What is
undocumented and unsound is that an **un-initialized-at-snapshot-time** global replays as `nil`: a value the
checker has statically proven impossible for an `int`/`List[int]`/struct-typed slot. Go (a goroutine
launched later reading a package-level var) and Python threads both see the current value.

**FIX (2026-07-25).** The staleness itself is gone, not just the `nil` hole — **each task snapshots the
module globals FRESH, pinned at its own `spawn`, at every depth**. Per-task isolation is unchanged.
Three increments at the one choke point (`ensure_snapshot`):
1. **`snapshot_memo` becomes a CACHE, not a forever-memo**, with exactly two invalidation rules:
   (a) a module-slot write (hooked in `set_global_slot` + `module_define` — the only two slot mutators);
   (b) `Op::EnterNursery`, when the cached snapshot is not `reusable` — i.e. some global holds a **mutable
   aggregate** (`ModuleSnapshot::reusable` / `slot_snapshot_reusable`, a conservative WHITELIST: scalars,
   `str`/`bytes`, `Func`/`Native`/`Builtin`/`Cffi`, the `Arc`-shared cores
   `Channel`/`Shared`/`RwShared`/`Atomic`/`Executor`/socket/`Writer`/`Reader`, and an import-alias
   `Module`). Rule (b) is what closes in-place mutation (`q.push(1)`, `m[k]=v`, `p.x=1`) between
   nurseries — it writes no slot for rule (a) to see — without touching the mutating intrinsics in the
   (then-fenced) `src/vm/call.rs`, and it keeps the cost at ONE rebuild per nursery instead of one per
   `spawn` (which is what the rejected second cut paid: 91× on a spawn storm).
2. **The cache + the snapshot became per-module-VIEW** (`FiberCtx`, swapped with
   `module_objs`/`module_faulted`), so a nested nursery inside a task snapshots the TASK's current view,
   and a shell draining several scopes faults each fiber from its OWN snapshot. Consequence: a shell no
   longer needs a snapshot at all → `spawn_shell` lost its `snap` parameter, deleting 5 `ensure_snapshot`
   call sites including both `.expect("no fault possible")` teardown panic vectors.
3. **A per-TASK PIN** (`QueuedTask.snap`), resolved EAGERLY in `Vm::register_task` — at the `spawn`
   itself, on both engines and on both the lazy and the EAGER (per-connection) path. The pin is a
   `Result<Arc<ModuleSnapshot>, RuntimeError>`: a build failure is CARRIED on the task and raised where
   the task is PREPARED, so a nursery whose tasks are all cancelled by a `break`/`return` stays faultless
   and the `parallel:` body's own output still precedes the fault (pre-W6-2 behaviour).

   Why per-TASK and not per-nursery: a bare `spawn` binds to the **implicit** nursery, whose
   `EnterNursery` the compiler emits at the TOP of the module/function body (`Span{1,1}`) and whose join
   is at the body's end. Any per-nursery pin therefore freezes an entire body at its first bare `spawn`
   — reintroducing W6-2's `nil` for every global declared later in that body (`spawn: …` / `n: int = 42`
   / `spawn: print(n)` → `nil`; a `List` global → `type nil has no method 'len'`). The first cut of this
   fix did exactly that and was rejected in review for it.

   Why EAGER and not "at the next slot write, else the join": the second cut deferred the pin to those
   two hooks and was rejected for **serial ≠ M:N**. The M:N EAGER nursery (a `parallel:`/bare `spawn`
   inside a running task — the `std.net` per-connection `serve` shape, gated on ≥2 hardware threads)
   PREPARES its task at the spawn, so it pinned there, while the serial engine queued and pinned at the
   next write or the join. An in-place aggregate mutation between the two instants writes no slot, so the
   engines snapshotted different views: `q: List[int] = [1]` + `spawn` + `q.push(2)` + `spawn` printed
   `first=2 second=2` on `--serial` and `first=1 second=2` on bare `run`, flipping back on `--threads=1`
   — i.e. output depended on the worker-pool width. The same deferral also (a) made every module-global
   write inside an open nursery scan the whole pending-task list (O(tasks × writes): 40k spawns + 40k
   writes went 0.083s → 1.761s), and (b) fired the hook while an `Executor` job's PRIVATE child module
   view was installed but the PARENT's task list was pending, handing the job's view to a sibling task (a
   task saw the job's `q.push(7)` on `--serial`, not on M:N — an isolation break too).

   With the pin resolved at the spawn, freshness comes from the two cache-invalidation rules, and the
   cache is what keeps the eager path cheap: a spawn storm inside one nursery builds ONE snapshot
   (asserted by build COUNT in `vm::tests::snapshot_cache_short_circuits_per_epoch_not_per_spawn`), where
   the second cut rebuilt per spawn (3000 eager spawns with a 20000-element `List[int]` global: 0.014s →
   1.272s, 91×). Rule 2 (`Op::EnterNursery` drops a non-`reusable` cache entry) is what makes a nursery —
   including a nested one inside a task that mutated its own copy in place — re-snapshot.

**Residual, documented** (`docs/concurrency.md` §2): in-place mutation of an aggregate global writes no
module slot, so it cannot refresh the cache mid-nursery. Within ONE nursery, consecutive `spawn`s share
one build, refreshed by a global ASSIGNMENT (rule 1) or by a new nursery (rule 2) but not by
`q.push(1)`/`m[k]=v`/`p.x=1`. So `spawn` → `q.push(2)` → `spawn` (same nursery, no assignment between)
gives the second task the pre-`push` view. Every task's view is ONE coherent instant (never a mix of old
and new values) and the same instant on both engines at every `--threads`; only its freshness stops at the
last assignment / nursery open. The between-nursery shape — the one that matters and the one this fix was
asked for — IS exact (`aggregate_mutated_in_place_between_nurseries`,
`map_and_struct_globals_in_place`), and the same-nursery residual is pinned by
`in_place_mutation_between_two_spawns_of_one_nursery` +
`nested_nursery_in_a_task_pins_at_its_first_spawn`.

**Cost measured** (best-of-3/5 wall clock, release binaries; the 9 `benches/run.chz` benches moved only
within noise — largest |delta| `loop` +2.6%, `map` re-measured 132.7 vs 134.3ms, none of them opens a
nursery):

| micro                                                                   | main    | this fix | 2nd cut (rejected) |
|-------------------------------------------------------------------------|--------:|---------:|-------------------:|
| 200k nurseries × 1 task, scalar/`str` globals only, `--serial`          | 0.598s  | 0.594s   | 0.608s             |
| …+ one 20-element `List[int]` global (the aggregate case)                | 0.799s  | 0.842s (+5.4%) | 1.000s (+25%) |
| 40k spawns + 40k global writes in one nursery, `--serial`                | 0.074s  | 0.090s   | 1.721s (23×)       |
| 2k spawns + 200k global writes in one nursery, `--serial`               | 0.026s  | 0.026s   | 0.231s (8.9×)      |
| 3000 EAGER spawns, 20000-element `List[int]` global, M:N (server shape)  | 0.014s  | 0.018s   | 1.272s (91×)       |
| the same nested shape on `--serial` (3000 tasks × 20000-element copies)  | 4.03s   | 4.46s (+10.6%) | 4.06s        |

Row 1 is the cache short-circuiting (ONE build for the whole run, asserted by count, not by timing).
Row 2 is the price of fresh-per-nursery for an aggregate global — one rebuild per nursery, as designed.
Rows 3–5 are the rejected cut's regressions, gone. Row 6 is the one measurable regression: the snapshot
is built at the first spawn instead of at the join, and on that pathological shape (10GB of per-task deep
copies) the changed ALLOCATION ORDER costs ~10%. It is not extra snapshot work — the build count is 2 in
both cases (measured directly), the peak RSS is identical (10.10 vs 10.10 GB), and disabling rule 2 or the
`install_snapshot` cache seed does not move it; the same shape with a realistically-sized global
(20 elements) is 0.015s vs 0.016s. Recorded rather than chased further.

**Note (pre-existing, NOT introduced):** `to_snap`'s slow arm re-attempts a full `to_wire` per level, so
snapshotting a module global deeper than `MAX_STRUCTURAL_DEPTH` (a ~5100-link recursive `struct Node:
next: Option[Node]` chain) is O(n²) — seconds in release, minutes in a debug build, and `main` behaves the
same. That is why the snapshot-BUILD-failure contract is gated by a white-box unit test
(`a_carried_snapshot_build_error_is_raised_at_task_preparation`) instead of a runnable fixture: the two
parity tests the rejected cut used for it ran source the CHECKER REJECTS (`deep: List[List[int]]` +
`deep = [deep]` → `cannot assign List[List[List[int]]] to List[List[int]]`) and only passed because
`run_program` skips the checker.

**FOLLOW-UP (not implemented, deliberately).** `src/vm/call.rs` was fenced while this landed (W6-3 in
flight; it has since merged), so the aggregate case is handled by the coarse whitelist rather than by
precise invalidation. The mutating intrinsics (`List.push`/`pop`/`insert`/…, map/set store, `SetField`, `SetIndex`) can
drop the cache only when the mutated object is reachable from a module slot, letting an aggregate-holding
program cache like a scalar one — which would also close the in-place residual above. Justified only if a
real workload shows the gap: the bar is row 2's **+5.4%** (a nursery-loop with an aggregate global)
shrinking to row 1's ≈0%, i.e. >5% of real throughput on a nursery-heavy program, not a micro-bench
alone. That same precise invalidation is also what would close the same-nursery in-place residual above.

### W6-3. A protocol method a built-in satisfies INTRINSICALLY is not callable at runtime — check-OK-then-run-fault, ~11 methods — P0 — **FIXED (2026-07-25)**
```chezzi
fn total[T: Add](xs: List[T], zero: T) -> T:
    acc := zero
    for x in xs:
        acc = acc.add(x)
    return acc
print(total([1, 2, 3], 0))
# check: ok  |  both engines: runtime error (line 4, col 15): type int has no method 'add'
```
Confirmed faults: `.add`/`.sub`/`.mul`/`.div`/`.mod`/`.neg`/`.hash` on `int`; the arith set on `float`;
`.hash` on `bool`/`str`/`bytes`; `.add`/`.sub` on a numeric newtype; `.index`/`.set_index`/`.slice` on
`list`/`map`/`str`; `.hash` on a zero-field struct. Also reachable WITHOUT generics: `x: Hashable = 5` then
`x.hash()` (check rc=0, same fault). **Controls green** — the operator forms all work (`a + b` on `T: Add`,
`-a` on `T: Neg`, `c[0]`/`c[0:2]` on `T: Index`/`Slice`), a real generic-`Hashable` Map key works (implicit
hash), `.compare()`/`.str()` on scalars work, and user structs defining the method work. So the break is
exactly the **explicit protocol-method call in an erased generic body** — the idiomatic Rust/Go shape.
**Root cause** — partial coverage, 2 of ~11 arms. `src/vm/call.rs:871` and `:885` are hand-written
interceptions for exactly `compare` and `str` on a scalar receiver. No sibling exists for the other
intrinsic grants: `src/checker/proto.rs:970` (`Hashable`), `:1028` (`Index`/`IndexSet`/`Slice`), `:1075`
(`Add`…`Neg`), `:1119-1155` (numeric-newtype operators), `:973` (zero-field-struct `Hashable`) — so the
receiver falls through to `has no method` at `call.rs:900` (scalars) / `:1367`,`:1416` (containers).
The grant site at `proto.rs:960` *documents the contract it doesn't uphold*: "the erased body's `v.str()`
is dispatched by the scalar `str` branch in `Vm::do_method_call`". Every intrinsic grant needs that pairing.
Reference: Rust `T: Add` makes `a.add(b)` callable — it IS the trait method; Go's interface method set is
likewise callable through the interface value. `std/prelude.chz:257` declares `Add.add` as the protocol's
method, so a type the checker says satisfies `Add` must answer `.add`.
**FIXED (2026-07-25) — every intrinsic grant now has a runtime arm, and the pairing is RATCHETED.**
One new `Vm::intrinsic_proto_method` (`src/vm/call.rs`) answers the whole set, and every arm **delegates**
to the exact primitive the operator form already uses, so equivalence is by construction (verified
observationally, both engines, value AND fault text): `add`/`sub`/`mul`/`div`/`mod` → `arith` (which
itself routes a same-newtype pair through `newtype_arith`, so the numeric-newtype grant needs no separate
code), `neg` → a new `Vm::neg_value` (`Op::Neg`'s body extracted verbatim into `src/vm/arith.rs`, now
single-sourced), `hash` → `hash_value` (**the Map/Set key hash** — so `x.hash()` can never disagree with
`m[x]`/`s.has(x)`, and a zero-field struct routes through `struct_hash`'s
`fields.is_empty() && !methods.contains_key("hash")` guard, the runtime mirror of `proto.rs`'s grant),
`compare` → `compare` (the underlying's NATIVE order, which is what `<` uses; on a NaN operand it answers
`sort()`'s total order via `order_key` — W6-3c, FIXED; see W6-3d for the one receiver where `compare`
still cannot match `<`), `index`/`set_index`/`slice` → `get_index`/`set_index`/
`get_slice` (with the `Option[int]` → raw `Nil`/`Int` unwrap `Slice`'s protocol signature requires, gated
on the fixed `VID_SOME`/`VID_NONE_VARIANT` ids). Nothing is reimplemented.
It is wired at **five MISS sites** in `do_method_call` — inline-scalar miss, the merged built-in-container
dispatch (`core_method`/`bytes_method`/`bytearray_method`, name-gated on the four container-intrinsic
names so an existing fault message like `Set.add`'s cyclic-key depth cap is never rewritten), struct miss,
newtype miss, and the catch-all `_ =>` (which is where a **boxed** `Obj::BigInt` scalar lands — it is
Obj-tagged and never reaches the inline-scalar arm). Miss-only ⇒ a user method always wins (it resolves
first) and the added per-call cost for an ordinary struct/handle method call is **zero**; the only
always-on change is that the three container-dispatch `matches!` probes collapsed into one `match`.
Benches re-measured: within run-to-run noise (the baseline itself flip-flops `loop` 1.01×↔1.00× and
`struct` 2.49×↔2.62× between samples); `struct`/`poly_method` neutral-to-better. Final sample (vs CPython,
lower is better): `fib` 2.99×, `struct` 2.48×, `poly_method` 3.75×, `list` 2.38×, `primes` 2.16×,
`str` 2.03×, `map` 1.57×, `loop` 1.07×, startup 4.5× **faster**.
Full grant↔arm pairing, now machine-checked: `Comparable`→`compare`, `Stringable`→`str`,
`Hashable`→`hash`, `Error`→`message`, `Iterable`→`iter`, `Index`→`index`, `IndexSet`→`index`+`set_index`,
`Slice`→`slice`, `Add`/`Sub`/`Mul`/`Div`/`Mod`→`add`…`mod`, `Neg`→`neg`.
**The ratchet** (worth more than the fix) is keyed on **(protocol × receiver KIND)**, because that is the
axis W6-3 actually failed on — `compare`/`str` WERE paired, but their interceptions were type-gated
narrower than the checker's grant set, so a protocol-keyed table could not have caught it. Three layers:
1. `checker::proto::Grant` — `satisfies_args_d`'s success type is a token with a private field, so a new
   early-out written the way every pre-existing one was (`return Ok(())`) does **not compile**; the author
   must pick `grant_intrinsic` (registers the grant) or `Grant::no_intrinsic_method` (documented as "this
   grants no callable method"). Verified: adding a bare `Ok(())` grant arm gives
   `expected \`Grant\`, found \`()\``.
2. `grant_intrinsic(protocol, ty)` `debug_assert`s that `(protocol, intrinsic_recv_kind(ty))` has a row in
   `INTRINSIC_PROTO_METHODS` (or `INTRINSIC_UNPAIRED`) — 51 paired rows + 0 carve-out rows
   (`INTRINSIC_UNPAIRED` is now EMPTY — W6-3b retired its only entry — but the const and its assertions stay
   so the ratchet re-arms the moment a new unpairable grant is added).
3. `vm::tests::intrinsic_grants_all_have_vm_arms` sweeps the **full (protocol × kind) cross product**
   (15 × 11 = 165 cells): it type-checks a `fn probe[T: P](a: T)` bound probe per cell and asserts the set
   of cells the checker ACCEPTS equals the registered row set, then RUNS a generated call probe per paired
   row on BOTH engines (and asserts every carve-out row still faults). Verified RED: adding `Ty::Bytes` to
   the `Comparable` grant — the review's exact trigger, and a widening the previous protocol-keyed ratchet
   passed — now fails with `intrinsic conformance granted for (Comparable, bytes) with no row`.
Not shipped, filed instead of silently held: only the numeric-newtype-with-its-own-operator-method
divergence (**W6-3d**, below). The other two are FIXED: **W6-3c** (`compare` on a NaN operand — it now
answers `sort()`'s total order) and **W6-3b** (`Iterator`→`next` on a raw collection — the grant was
narrowed to real cursors), both **2026-07-26**; see their sections.
Tests: `tests/chz/spec/intrinsic_proto_methods_test.chz` (20 `test fn` —
arith/neg/hash/index/set_index/slice/newtype/boxed-scalar/protocol-value, operator-equivalence AND
fault-message equality via `recover:`, plus user-method-wins controls, the W6-3d divergence pin and the
NaN total-order pin),
serial==M:N.

### W6-3b. `Iterator[E]`'s `next` was granted to a RAW collection but had no runtime arm — **FIXED (2026-07-26)**
The last `INTRINSIC_UNPAIRED` row is gone: `Iterator` conformance was narrowed from `iter_elem` ("can be
iterated") to "HOLDS a cursor position" — an `Iterator[E]` cursor (`.iter()` / a generator result) or a
struct with structural `next(self) -> Option[E]`. `fn f[T: Iterator[int]](c: T)` + `f([1, 2, 3])` is now a
TYPE error naming `Iterable` instead of a runtime `type list has no method 'next'`. A raw collection
satisfies only `Iterable` — the split Rust (`IntoIterator` vs `Iterator`) and Go (`range` vs an iterator
value) both make. The companion widening: element recovery (`recover_iter_elems`) now runs for
`Iterable[T]` bounds too, so `[S: Iterable[T], T]` is a drop-in for the iterating form and every shipped
caller (`examples/iterator_bound.chz`, `std.iter`'s `islice`/`imap`/`ifilter`) migrated with
byte-identical output. Recovery is NOT total for `Iterable`: an `iter()`-only struct still needs a
concrete-arg bound. `INTRINSIC_UNPAIRED` is now `&[]` (kept, with both `vm::tests` loops, so the ratchet
re-arms on the next carve-out). See `PROGRESS.md` (2026-07-26).

### W6-3e. `Iterable[T]` in TYPE position could not be iterated (the narrower `Iterator[T]` could) — **FIXED (2026-07-30)**
```chezzi
fn f(xs: Iterable[int]) -> int:      # accepted
    n := 0
    for v in xs:                     # type error (line 3, col 14): cannot iterate over Iterable[int]
        n += v
    return n
print(str(f([1, 2, 3])))             # …and the List[int] -> Iterable[int] argument ALREADY conformed
```
Check-OK-then-broken, and backwards: a raw collection satisfies `Iterable` but only a cursor satisfies
`Iterator` (W6-3b), yet only the narrower one worked as a value type. Root cause is a **representation
asymmetry**, not a missing string: `resolve_type` intercepts the reserved name `Iterator[T]` into
`Ty::Struct("Iterator", [T])`, while every other protocol name — `Iterable[T]` included — falls to the
generic-protocol arm and becomes `Ty::Protocol("Iterable", [Int])`. Both iteration unions matched only
`Ty::Struct(n, _) if n == "Iterator"`, so the annotated form fell to `cannot iterate over {other}`. Fix
(checker + ONE VM arm; the compiler is untouched — the `for` lowering is type-erased and branches at
RUNTIME on the heap `Obj`): one `Ty::Protocol(n, args) if (n ==
"Iterable" || n == "Iterator") && args.len() == 1` arm in `iter_elem`, and the two duplicated trailing
`for`-binding arms collapsed into one that consults `iterable_elem` (so the whole union is one predicate,
the wave-6 "fix applied to SOME arms of an N-way set" meta-finding). Every other consulter — the
comprehension arms, the `.iter()` fast path, `List()`/`Set()`/`Map()`, `satisfies(Iterable)` and
`recover_iter_elems` — routes through those two helpers and inherited it, so an `Iterable[int]`-annotated
param now also forwards into an `[S: Iterable[T], T]` bound.
**The VM half (the N-way set again, one rung down):** `iter_elem` gates `for` AND the
`List()`/`Set()`/`Map()`/`.iter()` consumers, but only the `for` lowering emits `Op::IterableToCursor`,
so those ctors inherited the STATIC acceptance without the runtime conversion — `List(xs)` on an
`Iterable[int]` param whose witness is an `iter`-only struct checked clean and then faulted
(`cannot iterate over struct (no `next` method)`) on both engines. The conversion is now a shared
`Vm::iterable_to_cursor` (`src/vm/stmt.rs`) called by BOTH `Op::IterableToCursor` and `drain_iterable`
(the declared runtime peer of `iter_elem`), so checker-accepts is again a subset of runtime-can-lower.
Fenced by `tests/chz/spec` `iterable_typed_iter_only_struct_feeds_every_consumer` (every ctor ×
the `iter`-only witness). `satisfies_args` grew ONE guard: a
`Ty::Protocol` subject now skips the intrinsic `Iterable` arm and is decided by the protocol-existential
arm (where the strict arg invariance lives), same as `Ty::Param` already did.
**Nothing widened**: `List[int]` → `Iterable[Any]`, `Iterable[int]` → `Iterable[Any]`, `List[int]` →
`List[Any]`, `List[Sq]` → `List[Shape]` and `Map[str, int]` → `Map[str, Any]` all stay REJECTED —
read-only covariance is deliberately not part of the model, **do not re-file it as a bug** (fenced by
`checker::tests::container_invariance_stays_rejected_for_iterable`). `Iterable[T]` still cannot call
`.next()` (W6-3b intact). Edge decided: an `iter`-only struct passed to a param ANNOTATED `Iterable[int]`
now WORKS (the annotation is the element type); the documented non-recovery limit is about BOUND position
and is unchanged — the "Known limits" line above was scoped, not deleted.
Tests: `checker::tests::iterable_*` / `container_invariance_stays_rejected_for_iterable` /
`iter_only_struct_bound_recovery_still_not_total`, and five `test fn`s in
`tests/chz/spec/intrinsic_proto_methods_test.chz` (list/set/map/str/cursor/generator/`next`-struct/
`iter`-only-struct, a comprehension, `List()`, and the stateful-cursor drain), serial==M:N.

**Round 2 (adversarial review) — the protocol-SELECTION half of the same N-way set.** The first cut
admitted a struct as `Iterable` by WELL-FORMEDNESS (`struct_iter_elem`, else fall back to
`struct_iterable_elem`'s `iter`) while the runtime picks by NAME PRESENCE (`iterable_to_cursor`: a
declared `next` ⇒ drive `next`, never convert via `iter`). A struct with a MALFORMED `next` (extra
params, or a non-`Option` return) plus a conforming `iter` was therefore admitted via `iter` and then
driven through the bad `next`: `viaList(Odd([9, 9], 0))` with `fn viaList(xs: Iterable[int])` returned
`[1, 2, 3]`, and a `next(self, k: int)` had `k` bound to nil (`drain_iterable`'s `run_proto` does not
arity-check) → `cannot apply Add to nil and int`. Identical on BOTH engines, so parity was blind to it.
Fixed by making the checker's rule the runtime's rule: `struct_iterable_elem` refuses any struct that
declares a `next`, so such a struct is non-iterable at check time (`syntax.md`, "`next` wins by NAME").
Two diagnostics were also widened wrongly along with the collapsed `for`-binding arm: a two-name
`for k, v` over an `Iterable[E]` ANNOTATION (or an `[S: Iterable[T]]` bound) reported "a struct iterator
binds a single loop variable" with no struct in the program — it now names the type
(`` `for k, v` requires a map, found Iterable[(str, int)] ``); a real struct keeps the struct wording.
Fences: `checker::tests::struct_with_nonconforming_next_is_not_iterable`,
`two_var_for_over_iterable_annotation_names_the_type`, and `tests/chz/spec`'s
`next_wins_over_iter_for_every_iterable_consumer` (a struct whose `next` and `iter` yield DIFFERENT
elements — every consumer must agree on `next`).

**Diagnostic-wording drift (cosmetic, not fixed)** — passing a concrete `str` into a `List[T]` inside
`fn f[T](xs: List[T])` reports "the collection's element type was pinned to `T` by an earlier push" when
it was pinned by the PARAMETER's annotation, not a push. No soundness issue. Distinguishing the two needs
provenance the site does not carry (`expr.rs`'s in-scope-`Ty::Param` branch was deliberately chosen in a
prior fix), so it is a real change, not a wording tweak.

### W6-3c. `Comparable.compare` on a NaN operand — **FIXED (2026-07-26)**: it answers `sort()`'s total order
```chezzi
fn cmp_m[T: Comparable](a: T, b: T) -> int:
    return a.compare(b)
nan := 0.0 / 0.0
print(nan < 1.0)        # false — the OPERATORS stay IEEE
print(cmp_m(nan, 1.0))  # -1 (x86) — the METHOD answers the total order, no fault
```
`float` is an intrinsically-granted `Comparable` type, but `compare(self, other) -> int`
(`std/prelude.chz`) has **no int encoding for "unordered"**: `<`/`<=`/`>`/`>=` all answer `false` for a
NaN operand (`Vm::ordered_bool`'s `None if both numeric => false`, IEEE-754/Python/Rust parity), and no
single int makes all four false. So `.compare()` cannot be observationally identical to its operator form.
The first cut raised a recoverable `cannot compare NaN (compare has no unordered result)` fault. That is
now **replaced by the total order the rest of the language already sorts by**: the `("compare", 1)` arm's
NaN branch (`src/vm/call.rs`) delegates to **`Vm::order_key`** — the single ordering site behind
`sort()` / `sort_by_key` / `.min()` / `.max()` (`f64::total_cmp`, NaN deterministically at one end,
numeric-`newtype` layers unwrapped first, so `Meters(nan)` behaves exactly like bare `float`).
The point is the **rule count**: there is now ONE total order shared by `compare`/`sort`/`min`/`max` and
exactly ONE documented divergence (total order for the method, IEEE for the operators) instead of two
orderings plus a fault. `docs/spec.md` already documented that total order for `sort()`; `.compare()` now
obeys the same rule. A generic `min`/`max`/`sort` written with `.compare()` therefore orders NaN data the
same way the `<` spelling's `sort()` does, instead of faulting on it.
Deliberately NOT changed: `Vm::compare`/`Vm::ordered_bool` (`src/vm/arith.rs`) — the operators stay IEEE,
which is the Python/Rust/IEEE-754 contract and no part of this fix. The protocol signature stays
`compare(self, other: Self) -> int`; the ledger's own candidate fixes (`compare -> int?`, an `Ordering`
enum with an unordered case) were rejected as milestone-sized and breaking for every `.compare()` caller.
Caveats, both pinned by assertion: `cmp_m(n, n) == 0` while `n == n` is `false` (`total_cmp` on identical
bits is Equal — the total order's definition), and only the **NaN** branch routes to `order_key`, so a
`±0.0` pair still answers via `self.compare` as IEEE-Equal (`cmp_m(-1.0 * 0.0, 0.0) == 0`) — i.e. the
shared total order is claimed for NaN, not for every float pair. The NaN END is target-dependent (the
signbit of `0.0/0.0` is negative on x86 SSE2 ⇒ NaN ranks below `-inf` ⇒ sorts FIRST, `compare < 0`), so
the test pins the ordering relative to `sort()` + antisymmetry rather than a hardcoded `-1`.
Pinned by `compare_on_nan_uses_the_total_order` in `tests/chz/spec/intrinsic_proto_methods_test.chz`
(both engines, byte-identical).

### W6-3d. A numeric `newtype` with its OWN `add`/`compare` disagrees with `+`/`<` — carved out of W6-3, low — **RESOLVED (2026-07-27) by ruling (a): the declaration is now REJECTED**
```chezzi
newtype Score = int:
    fn add(self, o: Score) -> Score:
        return Score(99)
    fn compare(self, o: Score) -> int:
        return 42
fn twice[T: Add](a: T, b: T) -> T:
    return a.add(b)
print(int(twice(Score(1), Score(2))))   # 99   <- the USER method
print(int(Score(1) + Score(2)))         # 3    <- the underlying's native op
# `cmp(a, b) == 42` (a > b) while `a < b` is true — a REVERSED order inside one bound
```
Pre-dates W6-3 (verified on the base binary, both engines) and is a genuine requirement conflict: the
intrinsic numeric-newtype grant is UNCONDITIONAL on such a method existing, intrinsic dispatch is
miss-only so a user method must win (never shadow one — the stronger rule), and the operator form always
auto-flows to the underlying's native op (a deliberate documented invariant, `docs/syntax.md`: "a
newtype's own `add`/`div`/`compare` is never dispatched as an operator"). Two spellings of the same
protocol operation therefore disagree for exactly this receiver.
Candidate fixes, all grant/design changes: (a) reject a numeric newtype that defines an operator-named
method (loudest, breaks any existing code that calls `.add()` deliberately); (b) make a numeric newtype's
own operator method dispatch as the operator too (drops the auto-flow invariant); (c) drop the intrinsic
grant when such a method exists, so conformance goes structural and BOTH spellings use the method (the
operator still wouldn't). Pinned as-is by
`newtype_own_method_wins_and_diverges_from_the_operator` in `tests/chz/spec/intrinsic_proto_methods_test.chz`
so whichever way it is resolved, the change is visible.

**ATTEMPTED AND REJECTED — candidate (b) makes `<` INTRANSITIVE (2026-07-26).** An auto-task run
implemented (b) (a numeric newtype's own `add`/`compare`/… dispatches as the operator too) on branch
`auto-task/newtype-op-method-dispatch`; it self-rejected after 2 remediation rounds, and BOTH blockers
were re-verified by hand on the branch binary vs `main`, on both engines. **The first blocker is
STRUCTURAL, not an implementation slip** — do not re-attempt (b) without resolving it:
```chezzi
newtype Ranked = int:
    fn compare(self, o: Ranked) -> int:
        return int(o) - int(self)          # a DESCENDING user order
fn lt[T: Comparable](a: T, b: T) -> bool:
    return a < b
xs: List[Comparable] = [Ranked(3), Ranked(1), 2]
print(lt(xs[0], xs[1]), lt(xs[1], xs[2]), lt(xs[2], xs[0]))
# main:   false true true   (total order)
# (b):    true  true true   <- a < b < c < a, a strict CYCLE
```
Cause: under (b) a SAME-newtype pair takes the user's (here descending) order, while a CROSS-type pair
under the `Comparable` existential (`Ranked(1) < 2`) cannot — the user's `compare(self, o: Ranked)` does
not accept an `int` — so it falls back to the native ascending order. One list then carries two orders
and transitivity is gone; `.min()`/`.max()` (which decide ONCE PER COLLECTION) keep answering
`Ranked(1)`/`Ranked(3)` while `<` (which decides PER PAIR) says every element is less than every other.
Any `<`-based algorithm (`std.bisect`, a user sort) inherits the intransitive comparator, silently, with
no fault. (b) is therefore incompatible with heterogeneous `List[Comparable]` unless such mixing is ALSO
banned for a compare-defining type — which is a strictly larger design change than the carve-out.
Second blocker (an ordinary regression, but it shows the checker-side cost): gating on the bound
protocol's `compare` second parameter being literally `Self` after substitution broke a protocol whose
`compare` takes the CONCRETE conformer type — `protocol OrdS: fn compare(self, o: S) -> int` with
`fn lt[T: OrdS](a: T, b: T): return a < b` prints `true` on `main` and is rejected on the branch with
`cannot compare T and T`. Branch discarded, not merged; `main` is unchanged and the divergence stands.
**This moves candidate (a) (reject the declaration) ahead of (b)**: it is the only candidate that makes
the two-orders situation unrepresentable rather than reconciling it after the fact.

**RESOLVED 2026-07-27 — ruling (a) landed.** A **numeric, non-generic** newtype may no longer define
`add`/`sub`/`mul`/`div`/`mod`/`compare`; it is a compile error at the DECL site
(`src/checker/setup.rs`, beside the existing static-method reject, which defers for the same reason —
the dispatch path does not exist):
```
type error (line 2, col 8): operator method 'add' on a numeric newtype is never dispatched as an
operator — a numeric newtype inherits int's operators, so '.add()' and the operator would disagree;
use a struct if you need your own arithmetic
```
Why (a) and not the others: **(b)** was implemented and rejected (the intransitivity above — a
STRUCTURAL conflict with heterogeneous `List[Comparable]`, not an implementation slip). **(c)** (drop
the intrinsic grant when such a method exists) makes `.add()` and the `[T: Add]` bound agree with each
other but leaves `+` still auto-flowing to the native op, so it narrows the hole without closing it.
Only (a) makes the two-orders state unrepresentable. It also matches the Go ancestor
([[no-drift-from-popular-languages]]): a Go defined type inherits its underlying's operators and Go has
no operator overloading, so the conflict cannot arise there — Chezzi manufactured it by letting the
protocol operation also be spelled as a method.
**`neg` is EXCLUDED from the reject list** — caught by adversarial review 2026-07-28 (charged
independently by all three prosecutors, upheld by the defender, who built both revisions and showed
`fn neg` compiled before the rule and errored after). Unary `-` has NO newtype path at all: `Neg` is
absent from the intrinsic grant and `satisfies`'s newtype arm returns `Err` for it, so `-m` on a
numeric newtype is already `cannot negate Meters`. With no operator to disagree with, a `neg` method
is the ONLY spelling of negation available — the first cut of this rule deleted working code and
justified it with a conflict that cannot occur. The rule now covers exactly the names a numeric
newtype genuinely *inherits* an operator for. Boundary pinned by the `ok(...)` case in
`checker::tests::numeric_newtype_ordinary_method_and_non_numeric_operator_name_still_ok`.
**Cost, accepted:** a one-way ratchet — any program deliberately calling `.add()` on a numeric newtype
stops compiling. Deliberately NARROW: ordinary methods (`fn doubled`) are untouched, and non-numeric
(`newtype Name = str`) and generic (`newtype Box[T] = T`) newtypes are unaffected — `satisfies` already
rejects the operator protocols for them, so there is no operator there to disagree with.
**Tests:** the reject is a compile-time diagnostic, so it is pinned in Rust —
`checker::tests::numeric_newtype_operator_named_method_is_rejected` (all seven names) plus
`numeric_newtype_ordinary_method_and_non_numeric_operator_name_still_ok` (the narrowness boundary). The
old Chezzi pin `newtype_own_method_wins_and_diverges_from_the_operator` asserted the divergence and
could no longer compile; it was REWRITTEN, not deleted, as
`numeric_newtype_operator_auto_flows_and_ordinary_methods_still_work` in
`tests/chz/spec/intrinsic_proto_methods_test.chz` — it now asserts the other half of the ruling (`+`,
`<`, the `[T: Add]` bound and an ordinary method all agree: `3 3 8 true`, byte-identical on both
engines). Docs: `docs/syntax.md` gained the rule + example beside the existing operator-protocol
paragraph.

### W6-4. `std.process` silently CORRUPTS non-UTF-8 child output (`from_utf8_lossy`), with no bytes hatch — the unswept B1/R1 sibling — P0 — **FIXED (2026-07-25)**
```chezzi
import std.process as pr
fn main():
    match pr.run("printf 'A\\377B'"):
        Ok(p): print("len=", p.stdout.len(), "bytes=", str(p.stdout.encode()))
        Err(e): print(e.message())
main()
# both engines: len= 3 bytes= b'A\xef\xbf\xbdB'      rc=0
# python3 subprocess: b'A\xffB'   (text mode raises UnicodeDecodeError rather than mangling)
```
**Root cause** `src/native/process.rs:34,39,58,59` — four raw `String::from_utf8_lossy` calls.
**This is definitively a defect, not a design choice**, because the identical pattern is tracked in this
file as **[B1](#b1-socketread-silently-corrupts-data-from_utf8_lossy--p0--fixed-2026-07-14-r1) (P0)** and
was ratified in R1 with a *different* answer: the `str` seam returns a **sticky `Err` that names
`read_bytes`** rather than mangling, and every affected module got a bytes twin (`Socket.read_bytes`,
`io.read_bytes`, `request.get_bytes`, `crypto.*_bytes`). `std.process` was missed by that sweep: no
`run_bytes`/`stdout_bytes` twin exists, `docs/stdlib.md` documents `stdout: str` with no lossy warning, and
this file had no entry. Go's `Output()` returns `[]byte`.
**FIXED (2026-07-25) — the hatch landed; the text seam stays a DOCUMENTED lossy view, on purpose.**
`process.run_bytes(line) -> Result[bytes]` and `process.run_args_bytes(prog, args) -> Result[bytes]`
(`src/native/process.rs`, declared in `std/process.chz`, both `Kind::Blocking`) return the child's stdout
**byte-exactly** on success. Their partition is **`cmd`'s, not `run`'s**: `Result[bytes]` carries NO
status channel, so **any failed child is `Err`** — a non-zero exit (stderr as the message, else
`command exited with status N`, the same rendering `cmd` uses, via a shared `failure_msg`) as well as a
spawn failure. That is the ratified R1 bytes-twin rule stated verbatim by `request.rs::lower_result_bytes`
("a non-2xx status here MUST become `Err` — otherwise a 404/500 HTML error page comes back as `Ok(bytes)`
and a caller writes it to disk as if the download succeeded"): `Ok(b"")` for a failed child would be
byte-indistinguishable from a successful command that printed nothing, so
`run_bytes("gzip -dc missing.gz")` would write a 0-byte file as if it had worked. A command that
legitimately exits non-zero *and* has meaningful stdout (`grep`, `diff`) belongs on `run`/`run_args`,
which carry `code` + both streams (shell form: `run_bytes("cmd; exit 0")`).
**Why the `str` seam does NOT Err the way `Socket.read` does.** The ratified B1 answer is not "Err", it
is **NON-DESTRUCTIVE**: `decode_carry`'s own contract says "a recoverable `Err` that silently drops
already-received payload would just be a different flavour of the corruption B1 fixes", and `Socket.read`
can only afford its strict `Err` *because* the undecodable bytes stay in `SocketCore::carry` for
`read_bytes` to hand back byte-exactly. A finished child has NO carry — its `Output` is already
consumed — so Err-ing `run` would DESTROY the captured stdout, stderr AND exit code (the bytes twins can
afford `Err` precisely because they have no `code`/`stderr` to destroy), and the advertised
"recovery" would be **re-running an arbitrary, side-effecting command line** (`git push`, a deploy, a
`timeout`). That is a worse failure than the U+FFFD it replaces, and it would also widen `run`'s
documented Ok/Err partition (`judge/run.chz` maps any `run` Err to a spawn-failure verdict). So
`std.process` follows the in-tree precedent for a CARRY-LESS seam instead: `request.get` keeps its lossy
`body: str` beside the byte-exact `request.get_bytes` — asserted on purpose by `request.rs`'s
`into_string_corrupts_but_get_bytes_is_exact`. The lossy decode is now stated at every statement of the
contract (`docs/stdlib.md` §std.process, the `process.rs` module doc, `std/process.chz`) with the
byte-exact twin named beside it, so nothing is *silent* any more.
**RESIDUAL (open, low):** the bytes path carries **stdout only** — no byte-exact stderr, and no
bytes-carrying structured result (binary stdout + stderr + code in one value). That needs a new native
struct/field through `seed_stdlib_structs` (`src/checker/setup.rs`) plus the two other hand-built
`ProcResult` layout copies; recorded in `docs/stdlib.md`'s "Not yet". No `2>&1` workaround is advertised:
splicing stderr TEXT into a byte-exact stdout stream would corrupt it, and `run_args_bytes` has no shell
to express it in.
Tests: `tests/chz/stdlib/process_test.chz` (byte-exactness of both twins, `Err` on a non-zero exit with
stderr as the message, `Err` on a spawn failure, and the text seam pinned as a lossy-but-non-destructive
view), serial==M:N. Shell lines in the suite single-quote their temp paths (a `TMPDIR` with a space or a
glob must not word-split — verified with `TMPDIR="/tmp/my dir"`).

### W6-5. A zero-field struct at an `extern` boundary PANICS the VM — `recover:` cannot catch it — P0 — **FIXED (2026-07-25)**
```chezzi
struct Empty:
    pass

extern "libc.so.6":
    fn abs(x: int) -> Empty

print(abs(1))
```
`check` → `ok: no type errors`. Both engines:
```
thread '<unnamed>' panicked at libffi-3.2.0/src/middle/mod.rs:129:10: low::prep_cif: Typedef
thread 'main' panicked at src/vm/mod.rs:4175:10: VM thread panicked: Any { .. }      rc=101
```
Wrapping the call in `recover:` still panics — it is **not** a recoverable fault. The param direction
(`fn abs(x: Empty) -> int`) is identical.
**Root cause** `src/checker/setup.rs:3023-3040`: `struct_fields_marshallable` loops over fields and returns
`true` **vacuously for an empty field list** — no zero-field reject. That reaches
`src/native/cffi.rs:163` (`CType::Struct{fields} => Type::structure(…)`) and libffi-rs's `Cif::new` unwraps
`prep_cif`'s `Typedef` error. C rejects an empty struct outright (GCC/Clang size-1 extension); either way
libffi cannot build a CIF for it. Fix: reject an empty field list where the other 7 marshalling rejects fire.

### W6-6. `struct X` + `extern fn X` SILENTLY calls the struct constructor — the guard is DEAD CODE, and the docs promise a reject — P0 — **FIXED (2026-07-25)**
```chezzi
struct strlen:
    s: str

extern "libc.so.6":
    fn strlen(s: str) -> int

print(strlen("hello"))     # -> strlen(s=hello)   <- the CTOR, not libc.  check rc=0, run rc=0
```
`docs/syntax.md:3024` promises the opposite in as many words: "An extern fn also may **not** be named after
… any of your `struct`/enum-variant names — those resolve to a special op before a plain call, so the extern
would be silently shadowed; **the checker rejects the collision**."
**Root cause — key-format mismatch.** The guard exists and runs (`src/checker/setup.rs:2798`,
`if self.structs.contains_key(name) || self.variant_owners.contains_key(name)`), but `extern_names` holds
the **bare** source spelling while `self.structs` is keyed **module-scoped** (`bare_key`/`type_keys`) —
proved directly: a marshalling error on the same struct prints `struct 'f4::S'`, so `contains_key("S")` is
always false. `variant_owners` IS bare-keyed, so the enum-variant half still fires — a one-file asymmetry
(`extern fn cosV` alongside `enum {cosV}` rejects; `extern fn sqrtS` alongside `struct sqrtS` passes).
This is the [[checker-test-helper-key-divergence]] class: the bare-keyed single-module `ok()` test helper
makes the unit test pass while the CLI graph path misses it.
**FIX AS SHIPPED — better than the `bare_key(name)` this entry originally proposed.** The sweep now keys off
`struct_names` (the BARE-visible ctor set, bare in BOTH paths) rather than `bare_key`-ing into `self.structs`,
because `seed_stdlib_structs` also parks **un-licensed** stdlib layouts (`Match`/`Response`/`ProcResult`/
`FileInfo`) in `self.structs` — so a `bare_key` lookup would have OVER-rejected `extern fn Match` in a file
that never imported `std.regex`. Pinned both ways: `extern_named_after_unimported_native_struct_ok` (accepted
without the import — nothing shadows it) and `extern_named_after_imported_native_struct_rejected` (the import
licenses the bare ctor, so the collision fires).
**AND the first cut of this fix was ITSELF partial-coverage** — caught by the adversarial review, confirmed by
hand on the real binary, remediated in `7abe925`. The new predicate enumerated `struct_names` +
`variant_owners` + builtin variants but omitted **`newtype_names`**: a newtype registers a bare-visible
one-arg ctor too, so `newtype abs = int` + `extern fn abs(x: int) -> int` checked OK and then called the CTOR,
printing `abs(-7)` instead of `7` on both engines. Lesson, third time in this file: **when you fix a
partial-coverage bug, enumerate the WHOLE set — the fix's own predicate is the next place the class hides.**
Test: `extern_named_after_newtype_rejected` (both decl orders, single-module + graph path, non-colliding control).

### W6-7. The `RwShared` zero-copy read-view is O(N²) — every GC re-walks the whole off-heap wire payload — **FIXED (found 2026-07-26, fixed 2026-07-27)**

> **Fix — one cached GC summary per wire core, computed at STORE time.** Every core
> (`Channel`/`Shared`/`RwShared`/`Atomic`/`Executor`) now carries `(approximate owned bytes, "can this
> payload root a heap object")`, derived by ONE new walk `crate::vm::core::wire_summary` (beside
> `collect_core_gcrefs`, arm-for-arm). `Heap::children` asks the summary first: a payload with **no
> `Handle` and no nested core** is skipped outright, so the per-GC-pass cost of a pure-data payload
> goes O(payload) → **O(1)**. A payload that CAN root is still walked in full, every pass, never
> memoized. `wire_summary` is deliberately **NOT** `WireValue::has_handle()` (`src/vm/wire.rs`): that
> one answers the *airlock* question and returns `false` for the nested-core arms that
> `collect_core_gcrefs` *recurses into* — caching its verdict would be a use-after-free. Here any
> nested core is unconditionally dirty and the walk stops at that boundary.
>
> **The trap this design had to survive:** `Shared`/`RwShared`/`Atomic` payloads are *replaced*
> (`set`/`update`/`write`/`store`/`exchange`/`cas`/`add`/`sub`), so a stale `CLEAN` after a store that
> introduced a handle would stop the GC tracing it. Four defences: (1) the queue cores' `queue` field
> is now **private** to `vm::core` — every push/pop must go through `ChanState`/`ExecState` helpers
> that maintain the summary, so a missed site is a *compile error*, not a review miss; (2) the
> single-value stores route through `SharedCore::store` / `RwSharedCore::store` /
> `AtomicCore::store`/`store_guarded`, which refresh the summary **under the same value lock** as the
> write; (3) a `debug_assert` in `Heap::mark_core_payload` re-derives the verdict on every debug-build
> GC pass, so any future store path that forgets to refresh trips the whole test suite; (4)
> `vm::heap::replacing_store_refreshes_the_gc_summary` drives each of those four store methods on an
> ALREADY-memoized-CLEAN core with a `Handle`-bearing payload and then mark-sweeps — deleting any one
> `summary.set` turns it RED (verified by mutation). The `Default`
> state is `WS_UNKNOWN` = "walk once, then memoize", so a core built outside a store path (the
> `..Default::default()` constructors in `src/vm/exec.rs`) degrades to the old behaviour rather than
> under-rooting.
>
> Note what defence (4) had to be, and why the *Chezzi-level* stress test
> (`vm::gc_tests::gc_stress_values_parked_in_cores`) cannot stand in for it: `WireValue::Handle` is
> produced by exactly one arm (`Obj::Module` → `sched.rs:2230`) and every core store funnels through
> `to_wire_crossable`/`wire_callable` → `ensure_crossable`, which REJECTS a handle-bearing value — so
> no program can park a `Handle` in a core, and the stress test's payloads are all provably CLEAN. It
> is a useful smoke test, not a proof; the memo's soundness is proven at the Rust unit level.
>
> **Measured** (`--serial`, release, same machine/session; the holder-isolation repro below, scaled by
> n — a 200k-int container held by X while a sibling loop allocates n times):
>
> | n | `RwShared` holder — before | after | plain `List` control |
> |---|---|---|---|
> | 100 000 | 0.447 s | **0.069 s** (6.5×) | 0.061 s |
> | 200 000 | 1.946 s | **0.203 s** (9.6×) | 0.196 s |
> | 400 000 | 7.916 s | **1.101 s** (7.2×) | 1.203 s |
>
> Before: 4.35× / 4.07× per 2× n — quadratic. After: the wire-payload holder **tracks the plain-`List`
> control at every n** (the control's own jump at 400k is a pre-existing heap-growth effect, identical
> before and after). Holder isolation at n = 200k: `RwShared` 1.766 → **0.218 s**, `Shared` 2.051 →
> **0.204 s**, `Channel.send` 2.050 → **0.220 s**, plain `List` 0.181 → 0.195 s, no holder 0.196 →
> 0.201 s — the holder penalty is gone on the GC/read side. The short-circuit alone restored
> linearity, so W6-7 needed no pacing change; pacing was later made byte-aware **for W6-10's sampling
> half**, but only when `--max-heap` is set (`mem_cap != 0`) — with no cap `next_gc` behaves exactly
> as it always has, so this table is cap-off and unmoved. Full table: `docs/benchmarks.md`. Tests:
> `vm::heap::core_payload_walk_is_memoized`, `dirty_core_payload_is_still_traced`,
> `live_bytes_counts_offheap_wire_payload`, `live_bytes_sums_every_distinct_core`,
> `vm::core::wire_summary_*`, `vm::gc_tests::gc_stress_values_parked_in_cores`.
>
> **Round-2 (2026-07-27) — the first cut had two regressions of its own; both fixed before merge.**
> (1) `Heap::live_bytes` de-duped cores by a linear `Vec::contains` scan re-run per core slot, so it was
> O(D²) in the number of DISTINCT live cores — and it runs on **every** `sweep()` (the `peak_live_bytes`
> probe, not gated on `--max-heap`). Same failure shape as W6-7 on a different axis, invisible to a
> microbench with one holder core and to `benches/run.chz` (no cores). K = 40 000 `Channel[int]()` +
> 500k allocations: base 0.102 s → 1.239 s. Fixed with `FxHashSet` (`src/vm/fxhash.rs`; `HashSet::default`
> does not allocate, so the no-core path is untouched) → **0.109 s, flat in K** up to 80 000.
> (2) The `wire_summary` walk ran INSIDE the value lock for `Shared`/`RwShared`/`Atomic` — for `RwShared`
> inside the EXCLUSIVE write lock, stalling every concurrent reader of the read view for a full payload
> walk per `set`. The channel paths already hoist theirs off `MnSched::core`; these did not. `*Core::store`
> now summarises the caller-owned value **before** taking the lock, and `AtomicCore::store_guarded` takes
> the pre-computed summary so `exchange` hoists too. Store-side cost remains (one walk per store, +21% on
> 50 × `RwShared.set` of a 100k list) and is now stated in `docs/concurrency.md` rather than claimed away.

<details><summary>Original report</summary>

Measured, `--serial` (M:N identical within noise), `for_each` over an `RwShared(List[int])` vs the same work
in a plain `for` loop:

| n | `RwShared.for_each` | plain `for` |
|---|---|---|
| 100 000 | 0.335 s | 0.058 s |
| 200 000 | 1.428 s | 0.154 s |
| 400 000 | 5.673 s | 0.579 s |

4× per 2× n — quadratic; the control is linear. At 1 M: 6.82 s vs 0.45 s. Isolated to the *holder* (same
200k-allocation loop, same live 200k-int container): plain `List` 0.107 s, `RwShared` 1.364 s (12.7×),
`Shared` 1.383 s, `Channel.send` 1.356 s, no holder 0.033 s — so it is the **wire-payload holder**, not the
read-view API itself.
**Root cause** `src/vm/heap.rs:627-651`: `mark_children` traces `Obj::Channel`/`Shared`/`RwShared`/`Atomic`/
`Executor` by calling `crate::vm::core::collect_core_gcrefs` (`src/vm/core.rs:296`) over the **entire**
stored `WireValue` tree on **every** GC pass — no "this subtree holds no `Handle`" short-circuit, no
memoization. And because the GC threshold is object-COUNT based (`next_gc = 2*live`) while a big wire
container is **one** heap slot, `live` stays tiny → GC runs constantly → cost is O(allocations × wire size).
`RwShared.for_each` allocates once per element via `from_wire` (`src/vm/netio.rs:2096`), so a walk is O(N²).
**Why it's a bug, not a known cost:** `docs/concurrency.md` sells the read-view as "fan a 1M-element shared
list out to 8 workers, each scanning/reducing in O(1) memory". Memory IS O(1); **time is O(N²)** and 10-15×
worse than not sharing at all. Go's `sync.RWMutex`+slice and Rust's `Arc<RwLock<Vec<_>>>` cost the runtime
nothing per traversal. Landed after the last perf pass, so no bench covers it. Same accounting seam as W6-10.
</details>

### W6-8. A STORED FFI callback dangles → SIGSEGV from checker-clean code (a "deferred" feature implemented as UB) — **FIXED (2026-07-27)**
```chezzi
extern "libc.so.6":
    fn signal(sig: int, h: fn(int) -> int) -> ptr
    fn raise(sig: int) -> int
fn handler(sig: int) -> int:
    print("handler", sig)
    return 0
h := signal(10, handler)
print(raise(10))
# check: ok    both engines: rc=139 (SIGSEGV, core dumped)
```
Stored/cross-thread callbacks ARE listed as deferred (`docs/syntax.md:2872`, `docs/ffi-and-packaging.md §1b`)
— but the deferral is implemented as **UB rather than a rejection**, and nothing in the checker flags a
callback param. Root cause `src/native/cffi.rs:104` ("the closure is freed before `call` returns") +
`CallbackClosure::drop` (`:541`) run at `:957`/`:1106`. Unlike CPython ctypes — where holding a reference to
the `CFUNCTYPE` object is a documented, achievable idiom — Chezzi offers **no way to keep the trampoline
alive**, so there is no correct program: every C API that retains a function pointer (`signal`, `atexit`,
GLib/GTK, `pthread_cleanup_*`) is a guaranteed segfault. A general check-time reject is impossible (the same
`fn(int)->int` param is legal for `qsort`), so the realistic options are keeping the closure alive for the
process (leak / heap-root it) or a loud doc + diagnostic. Precedent for taking FFI UB seriously:
[[ffi-callback-cif-heap-pin]].

**FIXED: leak the trampoline, POISON it.** Stored/cross-thread callbacks stay deferred — but the
deferral is now a **defined, loud abort** instead of undefined behavior. `CallbackClosure::drop` no
longer calls `libffi::low::closure_free`. It clears the ctx's `armed` flag (the exact inverse of the
arming store `Cffi::call` applies before `ffi_call`) and leaks
the `ffi_closure` allocation + the `Box<Cif>` + the boxed `TrampolineCtx` (fields are now
`ManuallyDrop<Box<…>>`). `callback_trampoline` checks `armed` **first** — before the
`ctx.host`/`ctx.params`/`ctx.ret` derefs and before `catch_unwind` — and on a cleared flag calls
`callback_poison_abort()`: a `write(2)` (retried, see below) of
`chezzi FFI: callback invoked after the extern call that received it returned; stored/cross-thread callbacks are not supported`
then `std::process::abort()`. Verified on the real release binary, **both engines**: was `rc=139`
(SIGSEGV, empty stderr), now that message + `rc=134` (SIGABRT). `examples/ffi_qsort.chz` (a
during-the-call callback) is byte-identical to its golden on both engines.

Four things are load-bearing and were each nearly a second bug:
- **All three allocations must leak, not just the closure handle.** libffi's generated trampoline
  derefs the prepped `ffi_cif` to marshal args and loads the userdata pointer BEFORE our Rust fn runs,
  so freeing the CIF or the ctx would just relocate the SIGSEGV into `classify_argument` — that is
  3038f67 / [[ffi-callback-cif-heap-pin]] again. `_cif` stays a `Box` **under** the `ManuallyDrop`; the
  compile-time guard in `boxed_callback_cif_address_is_stable_across_moves` now asserts `&**c._cif`,
  so reverting the field to a by-value `Cif` still breaks the build.
- **Guard PLACEMENT.** The old `ctx.host.expect(…)` sat INSIDE `catch_unwind`; leaving it there would
  turn a dead-owner invocation into a caught panic whose handler writes a `HostError` through
  `ctx.fault` — which points into `Cffi::call`'s `Box<Option<HostError>>`, freed when that call
  returned, so the write lands in freed heap. A quieter second UB.
- **`abort()`, not a panic or a Chezzi fault.** The realistic invocation site is a C signal handler
  (this very repro); unwinding from Rust into a C frame is itself UB, and Rust's stdio lock is not
  async-signal-safe — hence raw `write(2)` rather than `eprintln!`.
- **`qsort`-style during-the-call callbacks are untouched.** The `callback_fault.take()` re-raise still
  reads the fault BEFORE the drop on all three teardown sites, and the fix lives in the single `Drop`
  impl rather than per-call-site.

**Shape chosen: poison-in-place, not re-prep-to-a-stub.** Retargeting the live trampoline at a
VM-free stub via a second `ffi_prep_closure_loc` was considered and rejected: that call can return
`!= FFI_OK` on hardened W^X / static-trampoline platforms with no safe recovery (freeing restores the
UB; leaving the old trampoline pointing at a freed ctx is UB again), so a correct version of it is
"re-prep **plus** poison-in-place". Poison alone is the whole fix, is strictly smaller, adds zero work
to the `qsort` teardown path, and does not depend on undocumented libffi re-prep-in-place semantics.

**Only an ARMED trampoline leaks.** `Cffi::call` sets `ctx.armed` as its last act before `ffi_call`, so
a cleared flag at drop means the call bailed during arg marshalling (an
interior-NUL `str`, a return-only C type, a failed closure alloc for a later callback arg — all
`recover:`-able) and C provably never saw the code pointer. Those are still `ffi_closure_free`d. Leaking
them would make the cost per *attempt*, so a `recover:` retry loop that never enters C would grow the
pool for nothing (measured, pre-refinement: 200k faulting attempts leaked 72 MB and ~3100 mappings).

**Accepted ceiling (`ponytail:`-marked in `CallbackClosure::drop`):** one trampoline + CIF + ctx leaks
per **callback-passing** extern call — ~400 B of RSS, but it comes out of libffi's exec pool as a W^X
page PAIR, so it also consumes `vm.max_map_count` (~1 new VMA per ~130 calls; measured 200k `qsort`
calls → 90 MB peak RSS / 3168 VMAs vs a flat 11.5 MB / 46 before). A `qsort` in a hot loop therefore
grows memory *and* mapping count. **The exhaustion end of that is defined, not a crash:** the allocation
goes through `libffi::raw::ffi_closure_alloc` with an explicit NULL check, so a dry pool raises the
recoverable Chezzi error `cannot allocate a callback trampoline for argument N to 'f': the FFI closure
pool is exhausted`. `libffi::low::closure_alloc()` is deliberately NOT used: on failure it
`assume_init()`s a code pointer `ffi_closure_alloc` never wrote (uninit read = UB) and hands
`ffi_prep_closure_loc` a NULL handle to write through — i.e. the naive leak would have swapped a SIGSEGV
on an *unsupported* stored callback for a SIGSEGV on the *supported* during-the-call one. Upgrade path:
cache and reuse one trampoline per (closure identity, signature), freed when the owning closure is
collected. Callback-free extern calls never construct a `CallbackClosure`, so nothing else pays.

**The CROSS-THREAD half aborts too, and the guard is race-free.** A first cut poisoned by writing
`ctx.host = None` — a plain, unsynchronised write read by the trampoline from whatever thread C
invokes it on. That is a data race (UB regardless of the hardware), and a foreign thread observing
the pre-poison `Some` would deref a `*mut dyn Host` into `Cffi::call`'s dead frame: W6-8 again, just
narrower. Two changes close it:
- the armed flag is now an **`AtomicBool`** (`Release` on arm and on poison, `Acquire` in the
  trampoline), so the load/store pair is not a race and the arming writes are properly published.
  `ctx.host` is written ONCE, before C can see the code pointer, and never touched again — poisoning
  clears the flag instead of the pointer;
- an atomic still cannot stop a foreign thread reading a **stale `true`**, so the trampoline also
  compares `pthread_self()` against the `owner` recorded at ctx construction (write-once ⇒ no race,
  and `pthread_equal` is async-signal-safe) and aborts with
  `chezzi FFI: callback invoked from a thread other than the one that made the extern call; stored/cross-thread callbacks are not supported`.

Every combination is now defined: owner thread + during the call = the live path; owner thread +
after the call = `armed == false` by program order; any other thread, armed or not = abort. That also
covers the case the first cut left open (a C library that spawns a thread and calls back *while* the
extern call is still running) — it is unsupported by the [`CType::Callback`] contract, and now says so
instead of re-entering the engine off-thread. Demonstrated by widening the armed window with a 300 ms
sleep before the drop: pre-fix the C-spawned thread ran the Chezzi callback body and the program exited
`0`; post-fix it aborts on the cross-thread message.

**The abort DISCARDS the program's queued stdout, on purpose — draining it first deadlocks.** `chezzi
run` queues every `print` to a background writer thread (`src/vm/stream.rs`), so a bare `abort()` loses
whatever is still queued (measured: a 20k-line program truncates past the 64 kB pipe buffer, at a
run-dependent line). An earlier cut of this fix therefore drained the sink first via
`vm::flush_stream()`. **That was rejected in review and removed**: `flush_stream` is an unbounded
blocking rendezvous (`Msg::Flush(ack)` on an `mpsc`, then `rx.recv()` with no timeout) whose ONLY
servicer is the writer thread, so it wedges in two deterministic ways. (1) The poisoned trampoline
fires *on* the writer thread — an async signal (`signal(SIGALRM, h)` + `alarm`, SIGINT from the tty)
goes to any thread that has not blocked it, and `std::thread::spawn`ed writers inherit an unblocked
mask — so it queues a Flush for itself and waits on itself. (2) The writer is parked in `write_all` on
a full 64 kB pipe with no reader draining (`chezzi run p.chz | (sleep 60; cat)`), so the Flush queues
behind the stuck write. Either way the process HANGS: no SIGABRT, no exit status, no core — strictly
worse than the SIGSEGV this change exists to replace. `flush_stream`'s own contract already said so
(`src/vm/stream.rs`: "Called by `main` AFTER the VM has finished (never from a fiber)"); a C signal
handler is further outside that precondition than a fiber is. Independently, `mpsc::channel()` + `send`
both allocate and glibc `malloc` is not async-signal-safe, so a handler that interrupted an allocation
self-deadlocks on the arena lock. `callback_poison_abort` now calls nothing but `write(2)` and
`abort()`, both async-signal-safe. Losing buffered stdout on a crash is what every other runtime does
(CPython loses it on SIGSEGV/`abort`), and the diagnostic itself is never at risk — it goes straight to
fd 2 and never touches the queue.

**The message is written with a retry loop**, not one best-effort `write(2)`: on a non-blocking fd 2
(an inherited-`O_NONBLOCK` tty, a CI harness) or on a signal arriving mid-syscall, a single `write`
returns `EAGAIN`/`EINTR` and the process dies on a bare SIGABRT with EMPTY stderr — indistinguishable
from the SIGSEGV this fix replaces, and the message is the entire value of the change. `write_all_fd`
loops over short counts, `EINTR` and `EAGAIN` (1 ms back-off, ~2 s cap).

Tests: `tests/ffi_stored_callback.rs` — `stored_callback_aborts_loudly_on_both_engines` (the repro),
`cross_thread_stored_callback_aborts_without_entering_the_vm` (a `pthread_create` worker),
`abort_diagnoses_even_with_a_full_unread_stdout_pipe` (20k lines behind a pipe held unread across the
abort, polling for exit without draining — a re-added queue drain fails it as a 10 s timeout),
`unarmed_callback_trampoline_is_freed_not_leaked` (peak-RSS growth over 50k never-armed attempts — it
asserts the `/proc/self/status` probe actually WORKED, or the growth delta would compare two sentinels
and pass vacuously), and
`exhausted_closure_pool_faults_cleanly_instead_of_crashing` (the program caps its own `RLIMIT_AS` via
libc, drains the pool, and must get the clean fault rather than a signal), plus the `write_all_fd` unit
test in `src/native/cffi.rs` against a full non-blocking fd. All subprocess tests — the
first program dies on SIGABRT so it can never be a stdout golden, and FFI UB is layout-dependent. Each
child runs with `RLIMIT_CORE = 1` so a deliberately-aborting test never litters the host with core
dumps.

### W6-9. `Writer.write_bytes` is byte-exact on a file but LOSSY on `io.stdout()`/`io.stderr()`, and returns a count that doesn't match what was emitted — **FIXED (2026-07-27)**
`io.stdout().write_bytes(b"\xff\xfe")` emitted `ef bf bd ef bf bd` (two U+FFFD) and returned `Ok(2)`; the same
method on a FILE writer emitted `ff fe`. Python `sys.stdout.buffer.write(b'\xff\xfe')` and Go `os.Stdout.Write`
both emit the raw bytes. Docs: "`write_bytes(data: bytes) -> Result[int]` — Write **raw bytes**; returns
bytes written." **Root cause** `src/vm/fileio.rs:48-55` — the `Backing::Stdout`/`Stderr` arms of
`write_to_core` did `String::from_utf8_lossy(data)` because the `emit_out`/`emit_err` sink was `&str`-typed
(the comment conceded "the byte-exact common path is `write(str)`"). Same lossy class as W6-4 / B1 — the
last surviving member of the family.

> **The `&str` signature was the surface; `out: String` was the constraint.** `emit_out` routes to
> either `stream::write_out` (the streamed sink, `chezzi run`) or `self.out.push_str` — and `Vm.out`
> is the per-task buffer the whole serial-vs-M:N output-ordering seam is built on, recurring on
> `Vm`, `FiberCtx`, `WorkerResult` and all four `TaskOutcome` variants, moved through the M:N join
> plumbing and concatenated in task order by `reduce_task_slots`.
>
> **Fix** — widen the sink to bytes END TO END: `Msg::Write(Vec<u8>)` + `stream::write_out`/`write_err(&[u8])`
> (`src/vm/stream.rs`); new `Vm::emit_out_bytes`/`emit_err_bytes` holding the real logic, with
> `emit_out`/`emit_err(&str)` kept as one-line wrappers so the ~8 `&str` call sites (print,
> interpolation, natives) and the `Host` trait are untouched (`src/vm/exec.rs`); `out`/`stderr`
> retyped `String` → `Vec<u8>` on every struct above, `push_str` → `extend_from_slice` in
> `reduce_task_slots` with the slot ORDER untouched (`src/vm/sched.rs`); and the two `write_to_core`
> arms now pass `data` straight through (`src/vm/fileio.rs`). `Ok(data.len())` is unchanged and now
> truthful — no backing can short-write (`write_all` / in-memory / an unbounded queue) — and it is
> what Python returns too.
>
> **serial == M:N is preserved by construction**: concatenating `Vec<u8>` per task slot in the same
> index order is byte-identical to concatenating `String`. Nothing was sorted or normalised.
>
> **…and the ORACLE had to be widened with it (adversarial-review finding, fixed in the same entry).**
> The first cut left both parity oracles comparing the LOSSILY-DECODED capture, which is exactly the
> mechanism that hides a byte divergence: `from_utf8_lossy` is not injective, so a run whose serial leg
> emits `ff` where the M:N leg emits `fe` decodes to the same `U+FFFD U+FFFD` on both sides and
> `chezzi run --check-parity` printed `parity OK (serial == M:N)` with exit 0 — a detector degraded by
> the very feature it guards, and only reachable BECAUSE `write_bytes` went byte-exact. Fix:
> `vm::run_file_bytes` → `vm::RunOutputRaw` (the `RunOutput` shape minus the decode), taken by
> `run_check_parity` (`src/main.rs`) and by `assert_file_parity` (`src/vm/parity_tests.rs`), which now
> asserts the text (readable failure) AND the bytes. `--check-parity` also echoes the agreed capture
> with `write_all` instead of `print!`, so the tool reproduces the output of the command it checks, and
> its divergence report hex-dumps a line that is not valid UTF-8 (`serial: [fe, ff]` / `M:N: [ff, fe]`).
>
> **Residual, deliberate:** the CAPTURE boundary (`Vm::take_out`, the `run_*` helpers, `RunOutput`)
> still decodes with `from_utf8_lossy` in one shared `captured()` helper, because `chezzi test` and lib
> embedders hand stdout back to Rust as a `String`. A non-UTF-8 byte therefore still shows as U+FFFD
> *there* — a DISPLAY path — while the in-language contract, `chezzi run`, the only path a program's
> stdout actually reaches a console/pipe/file, is byte-exact. Widening `RunOutput` to `Vec<u8>` is the
> follow-up if an embedder ever needs it (~316 consumer sites for a display-only gain today).
>
> **CORRECTION (see `W6-9b`).** The claim above once read "the oracles no longer route through it" and
> that was FALSE when written: only the two comparators this entry names (`--check-parity`,
> `assert_file_parity`) were converted. Three MORE cross-engine comparators — `assert_parity` (the
> ~82-site capture path), `assert_parity_file`/`parity_entry` and `parity_entry_cfg` — kept diffing
> `captured()` output, so the majority of the parity suite stayed blind to exactly the divergence class
> `write_bytes` had just made reachable. That is filed and fixed as its own entry, **`W6-9b`** below;
> it is NOT covered by this entry's FIXED claim.
>
> Tests: `tests/interactive.rs::{stdout,stderr,buffered_stdout}_write_bytes_is_byte_exact_{mn,serial}`
> (real child processes — the only way to witness the bytes on fd 1/2, since the in-VM runner captures
> as a `String`) plus four in-language pins in `tests/chz/stdlib/io_writer_test.chz` (return count on
> stdout/stderr, the file arm's non-UTF-8 round-trip, a 200 KB write's full count), and two on the
> oracle itself in `tests/check_parity.rs`: a channel-ordered program whose engines emit `ff`/`fe` in
> different order must report DIVERGENCE with a non-zero exit, and an agreed non-UTF-8 capture must be
> echoed unchanged. The N1 dead-pipe
> contract (`emit_*` a no-op, `stream_halt` re-raised at the call site) is unchanged and still guarded
> by `broken_pipe_terminates_with_fault_{mn,serial}`.

### W6-10. `chezzi test --max-heap` does not count off-heap wire storage — 195 MB RSS passes a 200 KB cap — **FIXED in TWO parts (found 2026-07-26; accounting fixed 2026-07-27, sampling fixed 2026-07-27 round-3)**

> **TWO SEPARATE FAILURES, and the first commit only fixed one of them.**
>
> 1. **ACCOUNTING** — `live_bytes()` did not count a core's off-heap `WireValue` payload at all
>    (fixed first; the write-up below).
> 2. **SAMPLING** — `over_cap` is assigned ONLY inside `Heap::sweep()`, and `sweep()` runs only when
>    `Heap::should_collect()` fires, which was `self.since_gc >= self.next_gc` — a pure heap-OBJECT
>    count with `next_gc = (live*2).max(256)`. A program that pushes megabytes across the airlock
>    while allocating ~2 `Obj`s per iteration never reaches the object threshold, so it **never
>    sweeps, never samples the cap, and passes** — counting the bytes correctly changes nothing if
>    nobody ever looks. The round-2 review of this branch marked W6-10 FIXED on the accounting half
>    alone; that claim was **wrong**, and the shape that broke it is the natural one:
>
>    ```chezzi
>    test fn msg():
>        parts: List[str] = []
>        for i in range(100000):
>            parts.push("0123456789")
>        blob := "".join(parts)          # ~1 MB, built ONCE
>        ch := Channel[str](10000)
>        for i in range(300):
>            ch.send(blob)               # ~300 MB off-heap, ~2 heap allocs per iteration
>        assert true
>    ```
>    `chezzi test --max-heap=8000000 msg_test.chz` → **PASS, rc=0, peak RSS 304 MB** against an 8 MB
>    cap. Appending junk allocations to the same program flipped it to OVER-MEMORY, which is what
>    proved the discriminator was GC pacing, not byte accounting. Sibling shape (a 200k-int list sent
>    100 times, same cap): PASS at **3369 MB**. The earlier note that "GC pacing was deliberately left
>    untouched" is **retracted** — that declination is exactly what left the guard failing open.
>
> **Fix (sampling half) — byte-aware GC pacing, gated on a live cap.** `Heap` gained
> `since_gc_wire_bytes`, the `since_gc` sibling for growth that allocates no `Obj`s;
> `should_collect()` is now
> `since_gc >= next_gc || (mem_cap != 0 && since_gc_wire_bytes >= (mem_cap/4).max(64*1024))`, and
> `sweep()` resets it beside `since_gc`. The `cap/4` term bounds how far off-heap growth can overshoot
> between samples; the 64 KB floor stops a tiny cap from forcing a GC per store. The bytes are charged
> in `Vm::to_wire_crossable` (`src/vm/sched.rs`) — the one helper every cross-heap VALUE store routes
> through (`Channel.send`/`try_send`, `Shared`/`RwShared`/`Atomic` construct/set/update/store/CAS), so
> a new store path physically cannot forget the charge, the same argument that put `ensure_crossable`
> there.
>
> **Why the `mem_cap != 0` gate.** With no cap `over_cap` is meaningless, so the byte term exists only
> on the one path where it can matter: a cap-off run (every `chezzi run`, every bench, the whole
> serial==M:N parity gate) pays one `!= 0` load+branch per `should_collect` and ZERO extra walks, and
> pacing is bit-for-bit what it has always been. `mem_cap` is set once per test before the run and
> never changes mid-run, so the gated counter is never stale.
>
> **`since_gc_wire_bytes` is a pacing HINT, not accounting** — `live_bytes()` remains the sole measure
> of what is live. It is charged monotonically: a REPLACING store (`Shared.set`, `Atomic.store`)
> charges even though net live bytes may not grow, and a `recv`/`pop` never decrements. Net tracking
> would let a steady send/recv pipeline stall the trigger forever, i.e. fail OPEN again — the exact
> bug being fixed. Over-triggering costs an extra sweep under a cap and nothing else.
>
> **Accepted cost (measured, not claimed away):** the charge walks `wire_summary` a second time (the
> send path walks again when it caches the core's summary). Removing it would mean threading a
> precomputed summary through `MnSched::send_wake`'s signature for a CI/debug guard, so it was not
> done. Measured on a store-heavy program under a cap generous enough to PASS (200k-int list, 100
> sends, 4 GB cap, best of 3): **1.649 s → 1.828 s (+11%)** — the second walk plus the extra sweeps.
> Cap-OFF on the same program: 1.669 s → 1.676 s (noise). `benches/run.chz` and the W6-7 microbench
> are unmoved (both run cap-off; A/B of the two release binaries stays inside run-to-run noise).
>
> **Residual SAMPLING escapes (distinct from `W6-10r`, which is an ACCOUNTING hole):**
> - the documented inline-scalar case (`docs/future.md §1b`) — a loop growing one container of inline
>   scalars allocates no `Obj`s AND charges no wire bytes, so neither trigger fires. This fix does not
>   touch it. **CLOSED 2026-08-07 by `W7-28`**, which measured it at 77× the cap (not the 32× filed)
>   and found the same blindness in one-instruction `extend` and in few-allocation/huge-byte growth:
>   the trigger now charges BYTES at `alloc` / `get_mut` / `to_wire_crossable` instead of counting
>   events.
> - the by-hand airlock paths that pair `to_wire_at` + `ensure_crossable` instead of routing through
>   `to_wire_crossable` (spawn args, closure captures, `Executor.submit`) grow off-heap storage
>   without charging it.
> - pacing is PER HEAP under M:N, matching the existing per-heap cap semantics: a parent holding a
>   huge core but storing nothing still samples only on its own object churn. This narrows the escape;
>   it does not eliminate every shape.
>
> Tests: `vm::heap::wire_bytes_pace_a_sweep_only_under_a_cap` (cap-off ignores wire bytes / cap-on
> collects at `cap/4` / the 64 KB floor / `sweep()` resets) and
> `test_runner::over_memory_trips_without_object_churn` (both shapes above, both engines — each builds
> its payload ONCE, so object churn cannot be doing the work). Verified on the real release binary:
> the `msg` repro is now `OVER-MEMORY`, rc=1, peak RSS 15 MB (was PASS at 304 MB); the 200k-int
> sibling `OVER-MEMORY`, rc=1, 46 MB (was PASS at 3369 MB); the original 120000-list repro under
> `--max-heap=200000` `OVER-MEMORY`, rc=1.

> **Fix (accounting half) — `live_bytes` now counts the off-heap wire payload**, via the same per-core cached summary
> that fixes W6-7 (see above). `Heap::live_bytes`'s `_ => 0` blackout gained explicit
> `Obj::Channel`/`Shared`/`RwShared`/`Atomic`/`Executor` arms adding the core's cached byte count, so
> `sweep()`'s existing `over_cap = mem_cap != 0 && lb > mem_cap` finally sees a channel backlog / a
> list parked in a `Shared`. Queue cores keep the count incrementally at push/pop (O(message), next to
> the `to_wire`/`from_wire` already there — re-summing the whole queue per sweep would just be a
> different quadratic); single-value cores refresh it at store time.
>
> **What the number means: bytes REACHABLE FROM THIS HEAP.** A core's payload is ONE `Arc`
> allocation, but `from_wire` mints a FRESH `Obj::Shared`/`Obj::Channel` alias slot on every crossing
> (`src/vm/sched.rs:2641`), so a single heap can hold K alias slots for one core. `live_bytes`
> therefore charges each core's bytes **once per heap, by `Arc` pointer identity** — charging per
> *slot* multiplied a 100 MB payload by K and produced a spurious OVER-MEMORY at ~footprint/K, with
> the false-positive rate growing with fan-out (exactly backwards for a resource cap). A core shared
> by N M:N worker heaps still appears in each of them, which is correct for a per-heap *reachability*
> cap (each worker really can reach it) but means the N heaps' totals are not an ownership split of
> RSS. Test: `vm::heap::live_bytes_counts_a_shared_core_once_per_heap`.
>
> **RESIDUAL `W6-10r` — FIXED 2026-08-06 (was: the byte walk stops at a nested-core boundary).**
> Those bytes are owned by that core's own summary, and `live_bytes` reached a core's summary only
> through an `Obj::*` alias slot. A nested core whose last alias slot had been swept —
> `s := Shared(ch)` built inside a `fn`, so the local `ch` binding dies with the frame, then backlog
> through `s.get().send(...)` — survives inside the parent's `WireValue` with no slot of its own, so
> its backlog was counted **nowhere** and sailed past the cap exactly as before. The earlier claim
> that "that core's own summary owns those bytes" makes the case safe was **wrong** and is retracted.
>
> **Premise re-derived on the release binary BEFORE any edit** (the two preceding rows in this family
> had premises that had stopped being true — `W6-10s`'s filed escape turned out to be unreachable):
> the shape above at `--max-heap=8000000` measured **PASS, rc=0, peak RSS 304 MB**, while the
> identical program holding the channel in a live local measured **OVER-MEMORY, rc=1**. Confirmed as
> filed.
>
> **Fix — `core::nested_core_bytes`, the BYTE MIRROR of a recursion that already existed.**
> `collect_core_gcrefs` has always recursed into nested cores (a nested core may be reachable only
> through its parent, so its embedded handles would dangle otherwise); only `wire_summary`'s byte half
> stopped at the boundary. The new walk keeps the arms in lockstep with both, and two small helpers
> — `queue_bytes_deep` (shared by the identically-shaped `ChanState`/`ExecState`) and
> `value_core_bytes_deep` (`Shared`/`RwShared`/`Atomic`) — charge a core's own summary plus everything
> nested inside it. Three properties carry the fix:
> - **de-dup is SHARED with `live_bytes`'s own per-slot scan** — one `FxHashSet` of `Arc` pointers
>   spans both, so a nested core that *also* has an alias slot in this heap is charged exactly once
>   whichever way it is met first (charging it twice is the false-positive direction W6-10's review
>   already had to fix once), and it terminates `Arc` cycles.
> - **a `WS_UNKNOWN` summary is filled in passing**, exactly as `Heap::children` fills it while
>   marking. Every core CONSTRUCTOR (`Op::NewShared`, `new_atomic`, …) leaves the summary UNKNOWN and
>   a core reached only through a parent is never marked through an alias slot of its own — without
>   the fill it would report 0 bytes forever, i.e. the same hole with extra steps.
> - **gated on `mem_cap != 0`**, the same argument as the round-3 pacing counter: with no cap
>   `over_cap` is meaningless, so every `chezzi run`, every bench and the whole parity gate pay one
>   `!= 0` load and ZERO extra walks. Under a cap the cost is one O(payload) walk per **dirty** core
>   per sweep, on top of the mark pass's; a CLEAN core stays O(1). `CHEZZI_HEAP_STATS`'s cap-off peak
>   therefore still omits nested-core bytes — exactly as it did before the fix, so nothing regresses.
>
> Known ceiling, marked in the code: `dirty` conflates "holds a `Handle`" with "holds a nested core",
> so a queue of plain heap references is walked and finds nothing. Splitting them means a third field
> threaded through `WireSummary` and `ChanState::push`'s tuple at every call site — not built until a
> profile says it matters.
>
> Verified on the release binary: the repro **PASS @ 304 MB → OVER-MEMORY, rc=1, 16.5 MB**; the same
> program under a generous 4 GB cap and under no cap still **PASS** (no false positive). Tests:
> `vm::heap::live_bytes_counts_a_nested_core_with_no_alias_slot` (cap-off unchanged, cap-on charges
> the nested backlog, and adding an alias slot for the nested core does not double-charge) and
> `test_runner::over_memory_counts_a_nested_core_backlog` (the repro, both engines), plus
> `nested_core_bytes_walks_queued_messages_and_terminates_on_a_cycle` for the arms the headline test
> does NOT reach (a core nested inside a QUEUED message, and an A→B→A core cycle) — filed by review of
> this fix, since the headline nested core sits directly in a `Shared`'s payload over a pure-data
> queue and short-circuits before either. All **mutation-verified**: forcing the gate to `false` turns
> them red.
>
> **This closes `W6-10r`, NOT `--max-heap` in general.** Review of this very fix measured a sibling
> hole immediately: an `Executor`'s EAGER half — the only half the default M:N engine uses — is
> counted nowhere, and 300 submitted ~1 MB results PASS at **312 MB** against an 8 MB cap. Filed as
> **`W7-26`**, not bundled: it is a second payload half with no cached summary, not a nested core.
> (**`W7-26` is FIXED 2026-08-06**, and so is its own sampling residual **`W7-26r`** — including the
> pool-queue sibling filed with it.) The sampling residuals from `W6-10s` still stay open.
>
> **Observable change (the point of the fix):** `--max-heap` now trips where it previously passed.
> Nothing else moves — the dual-engine byte-identity gate runs cap-OFF, and `live_bytes` is otherwise
> only sampled for the peak probe. Test: `test_runner::over_memory_counts_offheap_wire_payload` (a
> `Channel` backlog and a `Shared`-parked list, both engines, under a cap far above anything either
> program keeps in its own `Heap` — so only the off-heap storage can reach it), plus the negative
> direction `under_cap_still_passes_with_many_handles_to_one_core` (50 reconstructed handles to one
> ~700 KB core under an 8 MB cap must still PASS — mutation-verified: removing the per-core de-dup
> turns it OVER-MEMORY).

<details><summary>Original report</summary>

A `test fn` that sends 120 000 `[i,i,i,i,i,i,i,i]` lists into a `Channel[List[int]](200000)`:
`chezzi test tw --max-heap=200000 -v` → **PASS**, rc=0, sampled peak `VmHWM` = 195 484 kB.
**Root cause**: the cap is `Heap::live_bytes() > mem_cap` sampled in `sweep()` (`src/vm/heap.rs:690`), and
`live_bytes` accounts only for in-`Heap` `Obj` slots and their owned `Vec`s. Values moved across the airlock
into a `Channel`/`Shared`/`RwShared` core live as `WireValue`s in an `Arc` **outside every `Heap`**, so they
are counted nowhere. **Distinct from the one documented escape** (`docs/future.md §1b`: "a loop growing a
single container of inline scalars … allocates no `Obj`s, never sweeps, and so never trips") — here EVERY
send allocates a `List` `Obj`, GC boundaries are hit constantly, and `live_bytes` is sampled hundreds of
times; the cap simply never sees the 195 MB. So the documented guarantee ("any single execution context
whose live heap exceeds `N` is aborted — a real runaway trips") is false for the most natural *concurrent*
runaway: an unbounded/large-cap channel backlog, or data parked in a `Shared`/`RwShared`. Same accounting
seam as W6-7. (The documented inline-scalar escape was separately re-confirmed and is NOT re-filed —
it is a DIFFERENT hole; it stayed open until **`W7-28` closed it 2026-08-07**, at a measured 77× the cap.)
</details>

### W6-9b. The serial==M:N parity oracle was only HALF byte-exact — the CAPTURE-based comparators still diffed a lossy decode — **FIXED (2026-07-28)**
Found by adversarial review of the W6-9 branch (charge "C1"), upheld by an independent defender that
reproduced it. W6-9 retyped the VM output sink `String` → `Vec<u8>` so `Writer.write_bytes` is byte-exact
on stdout, and in the SAME commit converted two comparators to diff raw bytes: `assert_file_parity`
(`src/vm/parity_tests.rs`) and the `--check-parity` CLI (`src/main.rs`). It did **not** convert the
capture-based path, which is the MAJORITY of the parity suite.

`captured()` is `String::from_utf8_lossy`, which is **not injective**: two engines emitting DIFFERENT
invalid UTF-8 (`ff fe` vs `fe ff`) both decode to the same two-U+FFFD string. Every comparator that
diffed `captured()` output therefore reported *parity OK* on a byte-divergent run. Three of them were
left blind:

| comparator | `parity_tests.rs` | reach |
|---|---|---|
| `assert_parity` (via `vm_outcome`/`parallel_outcome`) | `:18` | ~82 single-file sites (`run_capture` vs `run_capture_parallel`) |
| `assert_parity_file` / `parity_entry` | `:203` | the multi-file + std-module oracle |
| `parity_entry_cfg` | `:4077` | the `HostConfig` (args/env/stdin) oracle |

This was a **DETECTOR gap, not a live divergence** — no in-tree test emits non-UTF-8 through these
helpers, so nothing was failing. It matters because `write_bytes` going byte-exact is precisely what
CREATED the divergence surface, W6-9 is documented as having closed the class, and the remaining
blindness was therefore invisible. `tests/check_parity.rs::check_parity_reports_a_byte_only_divergence`
already proved such a program is constructible on this same path.

**Fix — strictly additive, at the HELPER level so no call site changed.** `src/vm/mod.rs` grew byte
siblings that hold the real bodies, with the existing `String` helpers demoted to one-line decode
wrappers: `run_program_bytes` (← `run_program`), `run_capture_bytes` (← `run_capture`),
`run_capture_parallel_bytes` (← `run_capture_parallel`); `run_program_inner` now returns `(Vec<u8>, …)`.
Every public signature (`run_capture`, `run_program`, `run_program_parallel`, `run_file_p`) is
UNCHANGED, so `src/vm/tests.rs`, `src/gc/tests.rs`, `src/checker/tests.rs` and `src/native/cffi.rs` are
untouched. In `parity_tests.rs` one shared `assert_stream_parity(a, b, what, label)` does **text compare
first** (a readable failure) **then the RAW BYTE compare on top** — the shape `assert_file_parity`
already used, whose body is now deduped onto it with its messages verbatim. `assert_parity` goes through
a new `assert_outcome_parity` over `vm_outcome_bytes`/`parallel_outcome_bytes`; `assert_parity_file` and
`parity_entry_cfg` take both legs from the existing `vm::run_file_bytes(..)` (byte-exact equivalents of
`run_file`/`run_file_p`/`run_file_with`/`run_file_parallel`, all of which are just
`to_str_output(run_file_engine(..))` — identical argument lists, `mk_cfg()` still called once per engine
for a fresh stdin queue) and return `captured(out)` so their `-> String` signature and every caller stay
put. **No existing assertion was removed, relaxed, sorted, normalised or made conditional** — the byte
`assert_eq!` is an EXTRA one after each existing one, and ordinary UTF-8 output is bit-for-bit
unaffected (byte-equality implies text-equality).

**Tests (failing-first, both RED before the fix):** `parity_tests::file_parity_catches_a_byte_only_divergence`
runs the channel-ordered fixture from `tests/check_parity.rs:137` through `parity_entry` under
`catch_unwind` and asserts it PANICS — the serial engine prints live (`fe ff`), M:N flushes task slots in
task order (`ff fe`), both decode to two U+FFFD, so only a byte diff sees it. It is a CANARY: if M:N slot
ordering ever changes so the two engines agree, it flips to failing — fix the ordering or the fixture,
do not weaken the compare (the CLI pin moves with it). The capture path cannot be reached by a real
program (`run_capture*` compiles via `compile_module_standalone`, no module resolution, hence no
`import std.io`), so its proof is the direct helper test
`parity_tests::outcome_parity_catches_a_byte_only_divergence` on `ff fe` vs `fe ff`.

> **Residuals, disclosed (`W6-9r` in the index table), all pre-existing and all UTF-8-only today:**
> 1. Hand-rolled `run_file_p` + `run_file` cross-engine compares still diff decoded `String`s.
>    Converting them means rewriting call sites, which the fix's shape constraint (helper-level only)
>    forbids. A future byte-emitting test added at one of those sites would inherit the blindness —
>    use `run_file_bytes` there. **CLOSED as WON'T FIX 2026-08-07.** Re-derived before deciding: the
>    count was under-recorded (~60 sites, not ~31 — `parity_tests.rs` plus `src/vm/tests.rs:8180`,
>    `:5166`, and `src/native/cffi.rs:2280`), and all of them are scheduled for **deletion** with
>    `--serial` (`future.md §2b` converts each pair to a single-engine M:N golden test, which compares
>    against a UTF-8 literal — where item 3's own finding applies and a decode hides nothing). The
>    standing instruction above still holds until then. The re-derivation found the same hole in the
>    oracle that OUTLIVES `--serial` — the CPython differential — fixed as **`W7-30`** (`:7408`).
> 2. `parity_entry_cfg_lines` (`:4100`) compares stdout as an order-insensitive line MULTISET
>    (`assert_same_lines`) and stderr as decoded text. The multiset is a pre-existing, deliberate
>    weakening (shared consumable stdin: which task reads which line is nondeterministic by design) —
>    left completely untouched so no reviewer can read a change near it as a loosened assertion. Same
>    for `assert_fault_same_lines` (`src/vm/tests.rs:489`).
> 3. `vm_outcome`/`parallel_outcome` keep the `String` shape. They are SINGLE-ENGINE assertion helpers
>    (~60 sites comparing against a literal / `contains` / the fn-pointer array at `:9900`), not
>    oracles — a decode cannot hide anything when the other side is a UTF-8 literal. Their doc comment
>    now says so, and points at `assert_outcome_parity` as the oracle.
> 4. The `captured()` DISPLAY boundary itself (`chezzi test`, lib embedders) is unchanged — that is
>    W6-9's own residual and stays open on the same terms.

### W6-11. `Ok`/`Err`/`Some`/`None`/`Result`/`Option` are accepted as `extern fn` names — same silent-shadow class as W6-6 — **FIXED (2026-07-25)**
`extern "libm.so.6": fn Ok(x: float) -> float` → 0 errors, unlike every other reserved name. `return Ok(x)`
still resolves to the variant, so the extern is unreachable by its own name.

### W6-12. `datetime.parse_iso8601` accepts a non-4-digit YEAR while every other field enforces exact width — **FIXED (2026-07-26)**
`"24-01-01"` → `year=24`; also `"4-01-01"`→4, `"024-01-01"`→24, `"20244-01-01"`→20244, `"202400-01-01"`→202400
(only >9 digits `Err`s). Python `d.fromisoformat("24-01-01")` → `Invalid isoformat string`. **Asymmetric
inside one function:** `"2024-1-1"` (2-digit month) correctly `Err`s. **Root cause** `std/datetime.chz:229-236`
— the year is guarded only by `all_digits` + `len() > 9`; month/day/hour/min/sec go through `field2()`
(`:200-203`, exact `len() != 2`). No `len() == 4` check on the year. Docs claim (`docs/stdlib.md`,
`parse_iso8601`): "matches Python `datetime.fromisoformat`" and "…**wrong widths**… are a clean `Err`".
**Corollary in the same cluster:** the documented total "Round-trips: `parse_iso8601(to_iso8601(dt)) == dt`"
is false at the top of the range — `to_iso8601(from_epoch(i64::MAX))` = `"292277026596-12-04T15:30:07Z"`,
which `parse_iso8601` rejects (`Err(year out of range …)`).
**Fix:** a `dc[0].len() < 4` guard beside the existing `> 9` cap. The bound MIRRORS the emitter
(`pad_year` writes >=4 digits, more for an extended year), NOT Python's exact 4 — a strict `== 4`
would reject what this module itself emits. The >9-digit corollary stands by design (the cap is the
`to_epoch` overflow guard); `docs/stdlib.md` now states the round-trip's real domain instead of
claiming it total. Tests: `t_parse_year_width` in `tests/chz/suites/datetime_test.chz`.

### W6-13. `datetime.days_in_month` returns 31 for ANY out-of-range month — **FIXED (2026-07-26)**
`days_in_month(2024, 13)` → `31`; same for month 0, -1, 100. Python `calendar.monthrange(2024,13)` →
`IllegalMonthError`. **Root cause** `std/datetime.chz:59-67` — the fall-through `return 31` has no
month-domain guard (the `# (1..12)` doc-comment is unenforced). Silently-wrong value: a plausible caller
(`if d > days_in_month(y, m)` date validation) accepts month 13 day 31.
**Fix:** a `panic("days_in_month: month out of range: …")` domain guard — the std idiom for a domain
violation in a non-`Result` helper (`std/string.chz:35`, `std/iter.chz:75`), recoverable via `recover:`,
and it keeps `-> int` so no caller changes. The only in-tree caller (`:289`) range-checks `m` first.
Test: `t_days_in_month_domain`.

### W6-14. `ffi.load_str` silently maps invalid UTF-8 to U+FFFD, undocumented — **FIXED (2026-07-26)**
Bytes `65 255 66` read back as codepoints `65 65533 66`. Same on the extern `str`/`owned_str`/`str?` return
path (a `strchr` landing mid-UTF-8-sequence returns a mangled `str`, no error). **Root cause**
`src/native/ffi.rs:435` (`load_str_impl`) + `src/native/cffi.rs:629`/`1005`/`1029` — `to_string_lossy()`.
Memory-safe but lossy. Go's `C.GoString` preserves the bytes verbatim; ctypes hands you `bytes` and raises
on `.decode()`. `docs/stdlib.md:662` states only a NUL-termination precondition. Same class as W6-4/W6-9.
**Fix:** `to_str()` instead of `to_string_lossy()` at all four sites, behind one shared message helper
(`ffi::non_utf8_err`) naming the bad offset and the `ffi.load_uint8_at` raw-byte hatch. Chosen over
doc-only for consistency with the IO contract (`Socket.read` already refuses a binary payload rather
than decoding it lossily); no `load_bytes` accessor — that stays its own milestone. `owned_str` still
frees the buffer BEFORE the fault propagates (no leak on the error path). Verified on the real binary,
both engines: `strchr("café", 169)` (a pointer landing mid-codepoint) now `Err`s instead of returning
a mangled `str`. Test: `load_str_rejects_invalid_utf8`.

### W6-15. `nan` loses per-element identity in containers (CPython drift) — **FIXED (2026-07-26)**
```chezzi
nan := (1.0e308 * 10.0) - (1.0e308 * 10.0)
xs := [nan]
print(xs == xs)          # true
print([nan] == [nan])    # false   <- CPython: True
print(nan in xs)         # false   <- CPython: True
print(xs.index_of(nan))  # -1      <- CPython: 0
```
CPython's container compare and `in`/`index` do an identity check per element before `==`. **Root cause**
`src/vm/arith.rs:1728` — the `if ha == hb { return Ok(true) }` shortcut sits inside the
`(ValueView::Obj, ValueView::Obj)` arm only; the numeric arm (`:1721-1723`) returns raw IEEE
`as_f64(l) == as_f64(r)` with no same-`Value` shortcut. Pre-existing (not from the 8B-`Value` or layout
work). Blast radius narrow: `float` is not `Hashable`, so NaN map/set keys are unreachable.
**Fix:** one `Vm::elem_equal` helper (`identity or ==`, identity being the raw `Value` word — a float is
heap-boxed per alloc, so one nan stored twice shares a box while two computed nans do not) used at every
ELEMENT compare: `seq_slot`/`set_slot`/`map_slot`, the recursive List/Tuple/Map/Set/Struct/Enum/NewType
arms, the set-op `in_set` walk, and `dedup` (which is defined by the same equality `in` is). The `==`
OPERATOR entry point (`arith.rs:176`, `exec.rs:1665/1671`) is deliberately untouched: bare `nan == nan`
stays false. The `RwShared` read-view walks (`netio.rs:2188/2230/2278`) keep plain `==` — their elements
are `from_wire`'d fresh per entry, so identity can never hold there anyway. Test:
`tests/chz/spec/nan_identity_test.chz` (3 of its 6 tests fail without the VM change; the two boundary
tests pass either way).

### W6-16..18 — cosmetic / diagnostic
- **W6-16 — FIXED (2026-07-25).** Duplicate diagnostic: `extern "libm.so.6": fn str(x: int) -> int` emitted
  the identical error **twice** (also `bytes`/`bytearray`/`Channel`/`List`/`Map`/`Set`), including under
  `--errors=json` → doubled LSP squiggles. Single for `int`/`float`/`bool`/`Shared`/`print`/`ord`/`chr`/
  `panic`/`range`/`timer`/`Executor`/`Atomic`. **Fell out of the W6-6 fix** rather than needing its own
  change: keying the collision sweep off `struct_names` (not `is_reserved_name`) means a reserved-callable
  name is reported ONCE, by the in-loop guard. Now single for every name in both lists.
- **W6-17 — FIXED (2026-07-26).** Turbofish over-rejected on the `RwShared` read-view's genuinely-generic `fold`/`fold_entries`:
  `r.fold[int](0, fn(a,x): a+x)` → `method 'fold' takes no type argument(s) (it declares no own type
  parameters)` + 2 cascaded infer errors, while the un-turbofished form works and harvested
  `[1,2].fold[int](…)` works. Sibling hole of "FIX 1a" above: `method_has_own_type_params`
  (`src/checker/expr.rs:1920`) answers from the harvested `self.structs` table, but the read-view methods
  from `cc07f77` are **arm-only** (`expr.rs:2751-2772`, E/K/V aren't nameable in `RwShared[T]`). Safe
  direction (over-rejection). **Fix:** the `Ty::RwShared` branch of that helper answers `true` for exactly
  `fold`/`fold_entries` before the table lookup — they already route through `infer_generic_method` WITH
  `type_args`, only the pre-gate rejected. Boundary kept: `rw.len[int]()` still rejects, and a non-container
  element still falls through to the resolver's "no method".
- **W6-18 — FIXED (2026-07-26).** `io.open()` on a DIRECTORY returns `Ok(Reader)`; the failure is deferred to every read, and
  `read_line`'s message advises `Reader.read_bytes`, which also fails (`Is a directory (os error 21)`).
  `io.read_file(dir)` correctly `Err`s at the call. Python `open(dir)` → `IsADirectoryError`.
  **Fix:** `io_open_reader` rejects an `is_dir()` handle at the call. The message text comes from a real
  1-byte probe read, so it is the OS's own wording — byte-identical to what `io.read_file(dir)` already
  emits (`/tmp: Is a directory (os error 21)`), not a second spelling of one condition. Test:
  `tests/chz/stdlib/io_open_dir_test.chz`.

### W6-19. A spawned task whose FIRST module-global access is a WRITE PANICS the M:N pool — host panic + serial≠M:N — P0 — **FIXED (2026-07-25)**
Found while fixing W6-2 (the mandated nested-nursery test could not even be written without tripping it).
```chezzi
g: int = 1
fn worker():
    g = 99                     # the task's FIRST touch of a module global is a WRITE
    print("worker g =", g)
fn main():
    parallel:
        spawn worker()
    print("parent g =", g)
main()
```
`--serial`: `worker g = 99` / `parent g = 1`, rc=0 (correct). Default M:N: `thread 'chezzi-pool' panicked at
src/vm/stmt.rs:1820: index out of bounds: the len is 0 but the index is 2` → `internal error: a parallel task
panicked`, rc=1. **The wave's one serial≠M:N divergence, and a host panic `recover:` cannot catch.**
**Root cause** `src/vm/exec.rs`: `Op::GetGlobalSlot` calls `ensure_module_faulted(home)` but the write arms
(`DefineGlobalSlot`/`SetGlobalSlot`) do not, so a worker whose modules fault in LAZILY indexed an empty
`slots` vec. **Fix:** one `ensure_module_faulted(module)` at the root, in `set_global_slot` — covering both
write ops and any future caller; free on the top-level/cooperative engines (no snapshot installed).
Regression: `parity_tests::spawn_task_first_global_access_is_write_parity`.

### Extra safe-direction observations — NOT filed as bugs
- `x: Any = if c: 1 else: 2.5` → `1.0`, and the `match`-expression form → `7.0` (also under an `Any` fn
  param / struct field / `List[Any]` element). Same design as the tracked wave-4 `List[Any]` gap and
  **explicitly documented** at `docs/syntax.md:416-417`+`1960-1966`. Noted only because
  `compiler/mod.rs:if_chain_numeric_mix` + `checker/pattern.rs:993` are a **second, non-container code
  path** for that corruption — if the deferred fix is scoped to the list peephole, it will miss this one.
- Two LATENT layout traps with no live trigger: `TID_NONE` collapses struct `==` type identity
  (`src/vm/arith.rs:1825` — two different *unregistered* struct types now compare equal if their fields
  match; before `c3b7b1c` the per-instance `name` distinguished them; unreachable today because every
  `NativeRet::Struct` producer names a registered key), and `rebuild_struct_names` assumes no duplicate
  `structs` key (`src/vm/op.rs:676-680` — an overwriting insert would silently leave the type name `""`).
  Traps for any future native that emits an unregistered struct name.
- Shift error says `shift amount 64 out of range (0..64)` — 64 IS rejected, so the printed range is wrong.
- `i64::MIN % -1` faults `integer overflow in Mod`; Go and Python both give a representable `0`.
- `fs.glob("d/*")` includes dotfiles, Python's `glob` excludes them — undocumented either way.
- `docs/stdlib.md` §`std.json` writes `decode[T](s)` as "a generic builtin", but the bare form is rejected
  (`'decode' takes no type arguments`) — the real spelling is `json.decode[T](s)`.
- `List[<numeric newtype>].sum()` is rejected at check while `.sort()`/`.min()`/`.max()` on the same type
  work — a post-fix asymmetry vs the 2026-07-23 numeric-newtype gap. Safe direction.
- Embedded-protocol method through an interface value is a **clean reject at check** (`type Person has no
  method 'name'`), not accept-then-fault — confirms the wave-3 observation is safe.
- `RwShared`/`Shared` nested same-box write (serial loses the inner write, M:N HANGS) — already tracked +
  documented as the reentrancy limit; re-confirmed, not re-filed.

### Domains that came back CLEAN (and are now no longer "never hunted")
- **GC + the freshly-rewritten object layout** (`c1f4d0e` inline ≤3 fields, `c3b7b1c` `Struct.name`→`tid`,
  `e66a1f5` mark bitset, `0100153` boxed `Obj::Module`, the 8B `Value`) — **~250 program runs + a
  220-program randomized differential fuzz: 0 divergences, 0 crashes, 0 wrong values.** Covered: the
  `Fields` inline/spill boundary (0,1,2,3,4,5 and 300 fields; megamorphic IC across 7 shapes straddling
  3/4), struct as Map/Set key both shapes with an allocating `hash` after 200k-alloc churn, self-referential
  and cyclic structs (depth cap faults *recoverably* on both engines and inside `chezzi test`),
  `tid`→name resolution (same-named structs in 2 modules, user structs shadowing `Match`/`Response`/
  `ProcResult`/`FileInfo`, generics/newtype/enum-payload, the `str`/`hash` hook home resolved *inside a
  spawned task*), boxed-module values, rooting under pressure in 10 holders (closure cell, `defer`,
  mid-`match`, operand stack across a method call, native re-entry, Channel buffer, `Shared`, `Atomic`,
  suspended generator frame), airlock of inline+spill+cyclic structs at `--threads=1,2,4,8`, and 8B-`Value`
  boundaries (`i64::MIN/MAX`, `-0.0`, `±inf`, NaN). Source-audited clean too: the `marks` bitset can't
  desync from `slots`, `ChzStr` SSO's `from_utf8_unchecked`, `run_until`'s `*const Program`, `str_intern`.
  **The two never-audited surfaces from the wave-5 residual are now swept** — GC came back clean; FFI did
  not (W6-5, W6-8, W6-14, and the extern-name holes W6-6/W6-11).
- **`RwShared` read-view CORRECTNESS** — 33 programs, serial==M:N on every one: nested read-views on the
  same box inside a `for_each` closure (no deadlock, no torn read — `3fedb34`'s per-element re-lock holds),
  mutation during the walk, all bounds cases byte-identical to plain indexing, **wrong constructor kind all
  rejected at CHECK** (`RwShared[int]`/`[str]`/`[MyStruct]`/tuple, `at`/`slice` on Map/Set, unconstrained
  `RwShared[T]`), faulting struct `hash` → clean recoverable `Err` with the box surviving (`04796a3`'s
  rooting holds), 600k allocations across 20 walks, concurrent writer vs walker on M:N, `slice` is a genuine
  deep copy, self-recursive `for_each` hits the recursion guard cleanly. Only the O(N²) *cost* is wrong (W6-7).
- **`chezzi test` selection/output flags + caps** — ~30 invocations: `-k`/`--filter` (substring, `Suite::method`,
  zero-match → rc=1 as documented), `--fail-fast`, `-q`/`-v` mutual exclusion, `--show-output`,
  `--errors=json` well-formed (`jq`) under a crashing test, a non-compiling file, and filenames containing
  `"`/`\`; `--timeout` shows the REAL cap for the body, a `recover:`-wrapped spin, a spawned task and a test
  with a `defer` (defer still runs, a runaway defer is re-tripped, an inner `recover:` can't swallow it, no
  deadline/marker leak into later tests); `--max-heap` recover-proof and correctly erroring with `--serial`;
  **identical verdicts `chezzi test --serial` vs bare `chezzi test`** on every suite built.
- **`Shared`/`Atomic`/`AtomicInt`/`RwShared` RMW** — 3 tasks × 2000 contended `update`/`add`/`write` →
  exactly 6000 each on both engines; `cas` and overflow faults correct.
- **stdlib breadth** — ~700 Python/Go-differential assertions: `std.path` (40 vs `posixpath`/Go
  `path.Clean`), string free-fn↔native-method pairwise parity (**~340 comparisons, zero mismatches**) + 20
  Python assertions, strings/bytes edges (~60: `\0`, combining marks, emoji, `ß`/`İ`/`ǳ` case ops, negative/
  reversed/OOB slices, `bytearray` aliasing), format specs + float `repr` (25, byte-identical to CPython
  f-strings incl. banker's rounding), numbers at every i64 boundary (13, all clean *recoverable* faults, no
  wrap), `std.math` (~35), collections (~45 incl. NaN total order, `sort_by` stability, Map insertion order
  across remove+reinsert, mutation-during-iteration snapshot), comprehensions (14), `std.json` (65),
  `std.regex` (17 — empty-match iteration follows Rust, no doc claim breached), `std.encoding`+`std.crypto`
  (24 known-answer vectors vs `hashlib`/`hmac`/`base64`), `std.duration` (29, Go `ParseDuration` parity),
  `std.flag` (24, Go `flag` parity), `std.collections`/`bisect`/`memoize` (25), `std.iter` laziness (20),
  `std.csv` (14 + **linear** scaling 4k/8k/16k rows, no O(n²)), `std.fs`/`std.os` (24), `std.io Reader` (12),
  core language (~60: `match` guards/range/struct patterns/as-expression, `defer` LIFO + latest-value,
  closure capture, `?` on both, `recover:` over 8 fault kinds, newtype/protocol/operator/static dispatch).
- **FFI sub-areas clean** (~60 probes): library/symbol resolution, boundary arity+type checking, `str` param
  edges (interior NUL caught; 20 000-char multibyte correct), fixed-width ints + a 21-arg CIF, ptr guards
  (8 assertions), ptr value semantics + identity across the airlock + a 300-node C-memory list under GC
  pressure, struct-by-value (SSE-class `cabs`/`conj` exact; all 7 documented rejects fire),
  `owned_str`/`str?` (no leak/double-free), **sync scalar callbacks** (9: fault re-raised and `recover:`-able,
  `defer` inside a faulting callback runs, 400-elem qsort under GC churn, nested re-entrant FFI, 200-deep
  recursion, captured-upvalue closure), **callbacks × concurrency** (4: 8 concurrent workers each running
  qsort-with-Chezzi-comparator, callback doing `Channel.send`, callback opening a nursery, callback blocking
  → clean deadlock error — all byte-identical serial vs M:N), airlock of FFI fn values (3 — confirms
  `f6e5ec3` is complete), native-decl seam (6 — `native fn` in a user file rejected; `ptr`/`int32`/
  `owned_str` correctly reserved; **no reserved-type hole**), extern nesting/duplicates (3).
- **Not probed, needs a helper `.so`:** C `_Bool` 1-byte marshalling, mixed INTEGER+SSE struct classes
  (`struct{int32; double}`), struct-by-value >16 bytes (hidden-pointer return), genuinely cross-thread
  callbacks. Also unspellable today: a **void-returning callback** (`fn(ptr, ptr)` without `->` is a parse
  error), which locks out most real C callback APIs — `docs/syntax.md §12b` never mentions this.

## Session log — 2026-07-24 (design consistency: `List.min()`/`.max()`/`min_by`/`max_by` fault on empty while sibling accessors return `Option` — OPEN, breaking change; re-confirmed 2026-07-26)

API-consistency drift found while documenting the test system (`[].min()` used as a fault-path
example). **Not a bug** (no crash — `min()`/`max()` raise a clean recoverable fault, `runtime error:
min() of empty list`, catchable by `recover:`), but a **magpie-lineage inconsistency** inside one
coherent method family:

- **The "element that might not exist" accessor family diverges by ancestor.** Verified current behavior:

  | method | empty → | return type |
  |---|---|---|
  | `.first()` / `.last()` / `.pop()` | `None` | `Option[T]` |
  | **`.min()` / `.max()`** | **faults** | **`T`** |
  | `.sum()` | `0` (identity) | `T` |
  | `[i]` index | faults (OOB) | `T` |

  `min`/`max` are the SAME category as `first`/`last`/`pop` — "return an element of the collection,
  which doesn't exist when empty" — yet they're the only ones that fault instead of returning `None`.
  Sigs: `std/prelude.chz:72-77` (`min`/`max` → `T`; `first`/`last`/`pop` → `Option[T]`).
- **Magpie check (an unintuitive divergence from the owning ancestor is a bug — [[no-drift-from-popular-languages]]).**
  Chezzi's `first`/`last`/`pop`-return-`Option` is the **Rust** model (Python has no such methods), so
  the family already chose Rust. Rust returns `Option` for `.min()`/`.max()` too; Chezzi's fault follows
  **Python** (`min([])` → `ValueError`) — a *different* ancestor for a sibling in the same family. Mixed
  lineage inside one family is the drift class. (`.sum()`→`0` is principled — `sum` has an identity
  element `0`; `min`/`max` have none, which is exactly why the no-value case wants `Option`/`None`, and
  the family already picked `Option` for no-value.)
- **Recommendation: `.min()`/`.max()` → `Option[T]`** (`None` on empty), matching `first`/`last`/`pop`
  and Rust.
- **Why OPEN/deferred — breaking change, own milestone.** Return type `T` → `Option[T]` touches:
  `std/prelude.chz` sigs; the VM `min`/`max` arm (`list_reduce_extreme`, `src/vm/call.rs:2021`, return
  `None` instead of `self.err("min()/max() of empty list")`); EVERY caller (now `.min().unwrap()` /
  `match` / `?`); tests; `docs/stdlib.md` + `docs/spec.md`. A checker↔runtime API-consistency fix, not a
  cleanup — schedule it as its own milestone with failing-then-green tests on both engines and a caller
  migration.
- **Re-raised 2026-07-26** (by the user, while batching the wave-6 fixes) and **re-confirmed OPEN** — the
  point stands that the `-> T` signature HIDES the fault, so a caller can write the crash without the
  type system saying anything. Two corrections to the scope above: the family also includes **`min_by`/
  `max_by`** (`std/prelude.chz:74-75`, same `-> T`, same `list_min_max_by` empty fault), and the caller
  migration is small — **23 call sites** across `std/`, `examples/`, `tests/`, `docs/`. Deliberately kept
  OUT of the wave-6 fix batch: that batch is behavior-scoped, this is a surface break.

## Session log — 2026-07-23 (bug-hunt wave 4: 1 finding — `List[Any]` mixed-numeric literal silently widens int→float — OPEN, deferred pre-freeze)

Adversarial pre-freeze hunt. (5 parallel subagents OOM-killed the box — `exit 137`, the cargo-memory-cap
gotcha — so 4 domains were cut off mid-hunt with only their probed sub-areas reported consistent; NOT a clean
sweep. One domain surfaced a lead, re-verified on the real binary.) One finding, **check-OK-then-wrong-value,
parity-blind** (both engines agree on the wrong value; not serial≠M:N):

- **`List[Any] = [1, 3.0]` silently stores `1.0` for the int — OPEN, DEFERRED past freeze.** `check` passes
  (element type resolves to `Any`, int `1` accepted as int); at runtime `str(xs[0]) == "1.0"` and
  `print(xs) == [1.0, 3.0]` on BOTH engines — Python keeps int `1` (`[1, 3.0]`). **Root cause
  (checker⊋compiler, type-blind compiler):** `src/compiler/mod.rs` `ExprKind::List` arm widens untyped int
  constants via the standalone `literal_numeric_mix` peephole whenever ≥1 float const sibling exists,
  *regardless of the checker-resolved element type*. The compiler only gets `float_elem_hint == Some(Elem)`
  for an explicit `List[float]`; an annotated `List[Any]` and an inferred-`List[float]` both arrive as
  `None`, so the peephole can't tell "keep heterogeneous" from "join to float" and widens both.
  **Blast radius is narrow:** only the TOP-LEVEL single `List[Any]` annotation leaks — the nested
  (`List[List[Any]]`) and `Map[str,Any]` paths make the checker infer the literal's *joined* type
  (`List[float]`/`Map[str,float]`) and cleanly REJECT the assignment. Control: `List[Any] = [1, 2]` (no float
  sibling) keeps int. No crash, no fault, no parity divergence — one wrong value under the `Any` escape hatch.
  **Why deferred:** the fix (checker sets `float_elem_hint` whenever it *resolves* element type to float —
  annotated OR inferred-join — and the compiler drops the standalone peephole, widening only on the hint)
  touches checker→compiler hint plumbing on the inferred-list path and must preserve inferred-`List[float]`
  widening while suppressing the `Any` case; a regression-prone hint change right before the JIT freeze is a
  bad value÷risk trade for a niche `Any`-escape-hatch shape. Revisit post-freeze if anyone hits it.
  (This RE-FRAMES the wave-3 "safe-direction observation" below — the asymmetry was noted, but the silent
  int→float *corruption* is new: the prior note only saw that `List[Any]=[1,3.0]` is *accepted*.)

## Session log — 2026-07-23 (bug-hunt wave 3: 4 findings — ALL FOUR FIXED; the residue is one untriaged safe-direction observation, see the end of this section)

Pre-freeze adversarial hunt, 5 disjoint domains (~248 probes, both engines). **3 domains CLEAN** (airlock 22,
cancel/defer/recover 37, checker⊋compiler ~40 — the productive class is exhausted). 4 findings survived
re-verification on the real binary — all **shared-wrong / check-OK type holes** the parity oracle is blind to
(none is serial≠M:N):

- **`Channel[T].trip()` typed `T` but always delivers `bool true` — check-OK type-soundness hole — FIXED.**
  `trip()` (the level-trigger latch behind `std.cancel`'s `done()`) was exposed on `Channel[T]` for all `T`,
  but recv/try_recv/wait unconditionally deliver `Bool(true)` (`vm/netio.rs`). On any `T != bool`, `check`
  passed then a `bool` leaked out of `recv()` where the type promised `T` (`Channel[int]().trip(); recv()`
  printed `true`; `recv()+1` faulted `cannot apply Add to bool and int`). **Fix — a new declarative language
  facet:** `where T: <scalar>` is now an **EQUALITY bound** (not a protocol) — the bound name may be a scalar
  type (`int`/`float`/`bool`/`str`/`bytes`/`bytearray`/`nil`), constraining `T` to be exactly that type.
  `trip()` gets `where T: bool` in `std/prelude.chz`, so the restriction lives in the `.chz` sig, not a Rust
  special-case. Implementation (checker-only, additive): `Checker::scalar_bound_ty` (proto.rs), a scalar-
  equality arm in `satisfies_args_d` + `check_bounds`, and the Channel method arm now calls `enforce_bounds`
  on the harvested `where_bounds` with `T→elem` (mirrors the `Ty::List` arm — Channel was the one container
  arm not wired for it). Tests: `checker::tests::{scalar_where_bound_is_equality_constraint,
  channel_trip_gated_to_bool}` + updated `channel_trip` in `reserved_method_tables_test.chz` (now `Channel[bool]`).
  Scoped to scalars (avoids generic-struct equality). `bound_provides` unchanged — a scalar bound constrains,
  provides no methods.

- **FIXED — native `"abc".count("")` returned 0** (Python/Go = `len+1`); the free fn `string.count` = 4. Commit
  `5a8fba0` fixed `std/string.chz` but missed the sibling native method (`src/vm/call.rs`, stale comment
  `// std.string: empty -> 0`) — the fix-one-caller-not-the-root miss. Fixed: empty branch now returns
  `s.chars().count() as i64 + 1` (codepoint len + 1). Test: `str_count_empty_sub` in `reserved_method_tables_test.chz`.
- **FIXED — native `"abc".split("")`** returned `["","a","b","c",""]` (leaked Rust's empty-pattern semantics; `call.rs`);
  matched neither Python (`ValueError`) nor Go (`[a b c]`), and its own sibling `std.string.split` `panic`s on
  empty separator. Fixed: an empty separator now raises a recoverable `split: sep must not be empty` fault (keyed
  on `sep`, so `"".split(",")` stays `[""]`). Test: `str_split_empty_sep_faults`.
- **FIXED — `Set.has`/`Map`/`in`/`List` on a cyclic struct key silently returned `false`** where `==` on the same
  two cyclic values faults `maximum structural depth (10000) exceeded` — self-inconsistent (Python raises
  RecursionError on both). Root cause: the `Vm::values_equal` wrapper (`arith.rs`) did
  `values_equal_guarded(l,r,0,span).unwrap_or(false)`, swallowing the recoverable depth `Err` into a wrong
  `false` at every container membership / key-equality site (~25). Fix: three `?`-propagating helpers
  (`seq_slot`/`set_slot`/`map_slot`) + inline `?`-loops replace the swallowing closures at every site
  (`arith`/`exec`/`stmt`/`call`, plus the `set_op` operator forms `\| & - ^` — signature grew a `span` +
  `Result` — and the `netio` Atomic `cas` compare); the wrapper is now `#[cfg(test)]`-only. A cyclic key
  now faults RECOVERABLY (byte-identical to `==`, Python RecursionError parity) on both engines. Also fixed
  a latent test-infra landmine: `chezzi test`'s SERIAL pass ran inline on the 8 MB main thread (M:N ran on
  the 384 MB VM stack) — a 10000-deep structural walk `SIGABRT`ed only there; both engine passes now run on
  `on_vm_stack` (matching `chezzi run`). Tests: `cyclic_key_faults_everywhere` + `noncyclic_controls` in
  `tests/chz/spec/map_set_test.chz` (bug-hunt wave-3 finding #4).

Safe-direction observations (NOT bugs — noted for a future look): protocol-embedded methods aren't callable
through the interface value (`p: Person` can't call embedded `name()`) despite spec.md:973 "flattened at bound
sites"; `List[Any]=[1,3.0]` accepted but `Map[str,Any]={"a":1,"b":3.0}` rejected (asymmetry vs spec's joint wording).
**[UPDATE — wave 4, above]** the `List[Any]=[1,3.0]` half is NOT safe: it silently corrupts the int to `1.0`.
**[UPDATE — 2026-08-06]** neither was the embed half: it was a plain bug (Go and pyright both accept
the program), and running the OWN-method control on each case found a SECOND defect this filing never
named — operators and `Self` on a protocol existential. Both **FIXED**; see the 2026-08-06 session log
at the end of this file. "Safe-direction" was an untested guess, not an observation.

## Session log — 2026-07-23 (bug-hunt wave 2 + completeness sweep: 3 fixes + 1 doc fix MERGED, 0 open findings, 2 dormant fragilities remain)

Pre-freeze adversarial hunt (5 disjoint domains, ~200 probes, both engines) + a **completeness/partial-coverage
sweep** (5 dispatch-table audits — "a fix/feature applied to SOME arms of an N-way set but not all"). All
findings verified on the real binary before filing.

**Bug-hunt (5 domains): all CLEAN** — airlock/capture (20), cancel/defer/recover (56), channel/nursery/
Shared/Atomic/Executor (34), checker⊋compiler (~35 int→float seams), stdlib+features (~50). Consistent with
6+ prior waves. One doc defect fixed: **`bytes(s)`** was documented (`docs/stdlib.md`) as UTF-8-encoding a
`str`, but `bytes()` rejects a `str` at check (Python's `bytes(str)` also errors without an encoding) — the
CODE is right, the doc lied; corrected to point at `s.encode()`.

**Completeness sweep (5 audits) — found the partial-coverage class in 3 spots:**
- **`order_key` missed the newtype-unwrap** → check-OK-then-run-fault on `List[newtype=float]`+NaN `.min()`/
  `.max()`/`min_by`/`max_by`/`sort_by_key`. Sibling of ff4d929 (which fixed `value_order`+`compare` but not
  `order_key`). **FIXED + MERGED** (753882d — its own session-log entry below).
- **Native/Cffi wire-path airlock** — the snap path shipped them but the wire path rejected them.
  **FIXED + MERGED** (f6e5ec3 — its own entry below). (This whole bug was the seed of the wave.)
- **Aliased native-struct import escapes reserved-type redeclare protection — RESOLVED-as-reframed
  (message fix).** The aliased case (`import Match as M from std.regex` + `struct Match:`) was never the bug:
  `M` is the imported name, `Match` is free, so accepting `struct Match` is CORRECT. The real defect was the
  UN-aliased case reporting the WRONG message — `import Match from std.regex` + `struct Match` said "type
  'Match' is reserved (builtin)" when these first-class Rust-bridged module-exported types are NOT reserved
  (a bare unimported `struct Match` is legal). It is an ordinary import-name collision, so it now reads "type
  'Match' is already defined" — aligned with the enum/newtype/typealias sibling arms, which already said so
  (they collide via `struct_names`). Fix: the struct hoist-guard (`src/checker/setup.rs:~2337`) moved
  `imported_builtin_types.contains(name)` out of the reserved branch and into the `already_defined` branch.
  Still a hard reject (no accept-then-trap); only the message text changed. Genuine global reserved types
  (`int`/`Channel`/…) keep "reserved (builtin)".

**Two DORMANT structural fragilities (no live trigger — not bugs, worth a cheap guard before freeze):**
- **Channel is the one native handle OFF the unified VM method-dispatch path** (`call.rs:989` `handle_key`
  match). The CHECKER-sig half of this gap is now CLOSED: `channel_method_sig` is retired and Channel's sigs
  are file-backed as a `native struct Channel[T]` in `std/prelude.chz` (harvested + resolved via
  `native_handle_method`, exactly like List/Map/Set/Shared/Socket). Only the VM-dispatch half remains — Channel
  still isn't in the unified `handle_key` match, so it isn't protected by the "add-a-handle-arm auto-enables
  bodied dispatch" guarantee the other 9 handles get. Self-consistent today (Channel has no bodied/generic
  methods); a future one would need a manual VM edit the structural guard won't force.
- **A non-handle native struct can harvest a bodied method the VM can't dispatch.** The compiler harvest
  (`compiler/mod.rs:~1086`) is generic over ALL native structs, but `try_native_bodied_method` is only reached
  from the 9-handle `handle_key` match — so adding a bodied `fn` to `Match`/`ProcResult`/`FileInfo`/`Response`
  would compile a proto into `native_methods[...]` the runtime never consults → check-OK/run-fault. None
  declare a bodied `fn` today. Cheap guard: assert every harvested `native_methods` key is reachable in the VM
  `handle_key` match, or restrict the harvest to reserved handle names.

**Audits that came back fully CLEAN:** airlock crossing-site guard coverage (single `to_wire_crossable`
chokepoint, no unguarded store); `stream_halt` dead-pipe re-raise + native Map-ordering (both "every X must
also do Y" contracts honored); method-dispatch handle×capability matrix (all 10 handles symmetric); the rest
of the NewType-unwrap surface (==, ordering, arith, hash, Display, casts, airlock, GC all newtype-transparent).

## Session log — 2026-07-23 (native/FFI fn values now cross the wire-value airlock — FIXED)

**Fix — native (`Obj::Native`) + FFI (`Obj::Cffi`) fn values are now sendable across the WIRE path.**
A native/FFI fn value passed `chezzi check` (its type is `Ty::Func`, checker-sendable) but FAULTED at
runtime when crossed via the wire-value path — `Channel.send(f)`, `Shared(f)`/`Atomic`/`RwShared`,
`Executor.submit`, `spawn use(f)` (fn-arg) — while the SAME value crossed FINE via the snapshot path
(`f := math.sqrt` captured by a `spawn:` block, `SnapValue::Native`/`Cffi`). Pure internal
inconsistency, not a fundamental limit: `to_wire_depth` (`src/vm/sched.rs`) lumped the two pure-code
arms (`Native`, `Cffi`) with `Module` into `WireValue::Handle(h)`, whose raw `GcRef` is meaningless on
another heap → `has_handle()` → reject. Fix mirrors the existing `Builtin` template + the shipping
`SnapValue::Native`/`Cffi` arms: two new by-value/by-`Arc` wire variants `WireValue::Native { name, func }`
+ `WireValue::Cffi(Arc<Cffi>)`, a split `to_wire_depth` arm (`Module` stays `Handle`; Native→by-value
fn ptr, Cffi→shared `Arc`), `from_wire` rebuild arms next to `Builtin`, and `collect_core_gcrefs` +
`display_wire` arms. `has_handle` needs no arm (they fall to `_ => false`). The `ensure_crossable`
diagnostic is corrected — only `a module handle cannot cross` now (Module is source-unreachable, so
it's a defensive-only guard). Verified serial == M:N byte-identical on Repro A (native via Channel /
Shared / spawn-arg / spawn-block) + Repro B (FFI `extern "libm.so.6"` via spawn-arg / Channel / Shared).
Tests: `tests/chz/spec/airlock_native_test.chz` (4 native `test fn`, gated both engines by
`chz_suite_passes_both_engines`) + the 5 flipped `ffi_handle_crosses_*` / `ffi_handle_send_succeeds`
parity tests (`src/vm/parity_tests.rs`). No checker/compiler/parser touch — the checker was already
correct; the runtime was the sole wrong gate.

## Session log — 2026-07-23 (bug-hunt: 1 finding — `std.path.ext` multi-leading-dot — FIXED)

Five-domain adversarial bug-hunt (airlock, cancel/defer/recover, channel/wait/Executor, checker⊋compiler,
stdlib) on both engines. **Four domains CLEAN** (~170 probes total, consistent with 6+ prior waves):
airlock/capture (30 probes — handles reject identically, data/closures/generators/cycles round-trip),
cancel/defer/recover (45), channel/wait/nursery/Shared/Atomic/Executor (34), checker⊋compiler (60 — no
accept-then-break; int-under-float stress-tested ~25 ways, all coerce-or-reject). One finding survived
re-verification on the real binary:

- **`std.path.ext`/`stem`/`with_ext` mishandle a name with MULTIPLE leading dots — shared-wrong vs Python
  (parity-blind), silent filename mangling — FIXED.** `ext` (`std/path.chz`) guarded only `dot <= 0` (a
  single leading dot), so a dot-only-prefixed basename split at its LAST leading dot instead of having no
  extension: `ext("..gitignore")` → `.gitignore` (Python `os.path.splitext` → `""`), `ext("..")` → `.`
  (Python `""`), and worst, `with_ext("..gitignore","bak")` → `..bak` — the `gitignore` filename was
  **silently dropped** (both `stem`/`with_ext` route through `ext`). The module's own doc comment claimed
  `splitext` parity ("a leading-dot-only hidden file has NO extension"), and `.bashrc`/`a.txt` were already
  correct → the intent was Python parity, the guard just under-skipped. Fix: after locating the last dot,
  return `""` unless some char in `0..dot` is a non-`.` (skip ALL leading dots, matching CPython
  `genericpath.splitext`). Both engines agreed on the wrong value → the parity oracle was structurally
  blind; caught by the CPython comparison. Regression: `t_ext`/`t_stem`/`t_with_ext`
  (`tests/chz/suites/path_test.chz`), gated serial==M:N by `chz_suite_passes_both_engines`.

**Two non-findings recorded (clean rejects, NOT soundness bugs — not chased):**
- **`str * int` string-repeat rejects** (`cannot apply * to str and int`) while `List * int` repeats — a
  Python-parity gap (Python `"ab"*3=="ababab"`), but a clean reject, not accept-then-break. Missing feature,
  not a bug — backlog candidate if string-repeat earns a milestone.
- **Float-sink if/match-expr asymmetry.** `x: float = 1 + 2` widens (→3.0), and the standalone if/match
  peephole widens int arms when a float *sibling* constant is present, but `y: float = if c: 1 else: 2`
  (all-int arms under a float context) **rejects** `cannot assign int to float`. Internal inconsistency but a
  false-*reject* (safe direction), and the spec only promises the sibling-constant peephole — defensible.

## Session log — 2026-07-23 (checker⊋compiler: numeric-newtype `.sort()`/`.min()`/`.max()` runtime gap — FIXED)

A numeric `newtype` (`newtype UserId = int`, `= float`) satisfies `Comparable` (the checker grants it by
the underlying's native order), so `check` ACCEPTS `.sort()`/`.min()`/`.max()` on a `List[newtype]`. But the
runtime comparators never unwrapped the `Obj::NewType` box: `Vm::value_order` (the `.sort()` comparator) fell
to `_ => Equal` → `.sort()` **silently no-op'd** (wrong result, no fault), and `Vm::compare` (the `.min()`/
`.max()` path) returned `None` → *"sort_by_key keys are not comparable: newtype vs newtype"* fault. Both
engines behaved identically → the parity oracle was structurally blind (a checker⊋compiler class: check-OK,
run-divergent). Bare `<`/`>` already worked (`compare_op` unwraps same-newtype inners).

**FIXED (both `src/vm/arith.rs`):** added a newtype-unwrap arm at the top of both `value_order` and `compare`
that reads `Obj::NewType.inner` and recurses on the wrapped scalar — one side per call converges to scalar
operands, so it covers both-newtype (the homogeneous-list case), the defensive one-side case, and nested
`newtype B = A`. Orders by the underlying's *native* scalar order — exactly matching bare `<` (`compare_op`)
and the checker's Comparable grant. `value_order`/`compare` are `&self` and structurally cannot re-enter
`run_proto`, so recursing on the inner scalar (never a user `compare` method) is the only consistent choice.
Regression: `tests/chz/spec/newtype_test.chz` (sort/min/max on `List[newtype=int]` + `List[newtype=float]` +
bare `<`/`==` + `sort_by_key` positive controls), gated serial==M:N by `chz_suite_passes_both_engines`.

**Clarification (not a bug):** a `str`/`bool` newtype does **not** satisfy `Comparable` (checker grants it for
numeric underlyings only), so `List[str-newtype].sort()` is rejected at `check` and never reaches the runtime.
The str-inner unwrap is present for free (lands in the existing `Obj::Str` arm) but is not source-testable.

**Follow-up (same day) — `Vm::order_key` was MISSED (partial coverage):** the `.min()`/`.max()`/`.min_by`/
`.max_by`/`.sort_by_key` path routes through `Vm::order_key` (`src/vm/call.rs`), a *separate* comparator from
`value_order`/`compare` — and it was **not** unwrapped by the fix above (the "covers `.min()`/`.max()`" claim
was mis-attributed; only `.sort()` via `value_order` was actually covered). So a `List[newtype=float]` key
containing a `math.nan` still faulted *"sort_by_key keys are not comparable: newtype vs newtype"* at `.min()`
(a wrapper is `Obj`-tagged, so `order_key`'s `is_float`/`is_numeric` NaN net both miss it → fault arm). **FIXED:**
mirrored the `value_order` newtype-unwrap arms at the top of `order_key` (after the `Struct`/`Struct` arm,
before the `is_float` fast-path; copies `*inner` to a local first to release the `heap.get` borrow before the
`&mut self` recursion). This also closes a benign `-0.0`/`+0.0` inconsistency the two paths had (`sort()` used
`total_cmp`, `min/max` used `partial_cmp`) — `order_key` now routes newtype floats through `total_cmp`, matching
`sort()`. Regression: `minmax_nan_float_newtype` + `by_key_nan_float_newtype` in `newtype_test.chz`, gated both
engines. No checker/`value_order`/`compare` change — `order_key` was the sole gap.

## Session log — 2026-07-22 (bug-hunt: 2 findings — 1 fixed, 1 pre-freeze known-limit + serial-removal plan)

Five-domain adversarial bug-hunt (airlock, cancel/defer, channel/wait/Executor, checker⊋compiler, stdlib) on
both engines. **airlock**, **cancel/defer/recover**, and **checker⊋compiler** came back **clean** (21 / 18 /
40 probes; consistent with 6+ prior waves). Two findings survived re-verification on the real binary:

- **`string.count(s, "")` returns 0; Python & Go return `len(s)+1` — FIXED.** `std/string.chz` guarded
  `if m == 0: return 0` — drift from **both** ancestors (`"abc".count("") == 4` in Python and Go) and
  inconsistent with its own sibling `index_of("abc","") == 0` (Python-correct) in the same module. Fixed to
  `return s.len() + 1` (`s.len()` is codepoint length, matching Python). Both engines agreed on the wrong
  value → the parity oracle was structurally blind (shared wrongness); caught by the CPython/Go comparison.
  Regression: `string_count_empty_substring_matches_python` (`src/vm/parity_tests.rs`, `parity_entry`).

- **`wait:` timer arm makes `--serial` inline-sleep instead of yielding → serial ≠ M:N — PRE-FREEZE
  KNOWN-LIMIT (N10).** See [N10](#n10-a-wait-timer-arm-makes---serial-inline-sleep-instead-of-yielding-to-a-runnable-sibling--serial--mn--pre-freeze-known-limit-found-2026-07-22-fix-deferred-to-the-post-freeze-serial-removal).
  The shipping M:N engine is correct; the serial oracle diverges. Fix deferred because the serial engine is
  **slated for post-freeze removal** (below).

**Post-freeze serial-removal + oracle-layer plan recorded** (`docs/future.md` §2b). Rationale: `--serial`
(a) *can't truly test concurrency* (single-threaded, can't preempt — N8/N9/N10), and (b) *keeping it
byte-identical to M:N is accruing debt* (per-engine split branches that exist only to keep serial matching
M:N). Post-freeze it is removed and its oracle job re-covered by a layer: **CPython differential** (sequential
shared wrongness, already built) + **Go paired-programs** (channel/`select` semantic wrongness, deterministic-
outcome only — Go can't oracle the airlock/nursery) + **seeded/deterministic-interleaving M:N** (races — the
real replacement for serial's race-finding; an external lang's scheduler is also nondeterministic so it can't
do this) + **hand-written known-answer** (airlock semantics). Together they cover more than serial==M:N did,
without the byte-identity tax. The JIT freeze is the cut point (post-JIT, serial byte-identity is impossible).

## Session log — 2026-07-20 (bug-hunt: 4 findings — 2 checker fixes + 1 doc fix, 1 held)

Five-domain adversarial bug-hunt (airlock, cancel/defer, channel/nursery, checker⊋compiler, stdlib) on
both engines. Airlock, channel/nursery, and stdlib came back **clean** (35/19/46 programs; consistent
with 5+ prior waves). Four findings survived re-verification on the real binary:

- **F1 — `?` in a `defer:` block over-rejected by the enclosing fn's return type — FIXED.** The `defer:`
  block is its own closure with a `?`-DISCARDING contract (`syntax.md`: "a `?` short-circuit inside the
  block is discarded"), but `infer_try` (`src/checker/pattern.rs`) validated the `?` against the enclosing
  `current_ret`, so `defer: v := g()?` **rejected** under a nil/int-returning fn and only **accepted**
  under a `Result`-returning one *by coincidence* (wrong model — the runtime discards, never propagates
  to the enclosing return). Fix: an `in_defer_block` checker flag (mirrors `recover_depth`; saved/reset
  at every fn/closure boundary, and zeroes `recover_depth` on entry — the block can't target an outer
  `recover:`). When set, `infer_try` discards the `?`: accept any `Result`/`Option`, yield the success
  payload, no enclosing-return constraint; a non-sum operand still rejects. Checker-only, parity-neutral;
  runtime discard verified byte-identical on both engines. Tests: `defer_block_q_discards_regardless_of_
  enclosing_return`, `..._still_rejects_non_sum_operand`, `fn_declared_in_defer_block_gets_own_q_context`
  (checker) + `defer_block_q_discards_fired_err_parity` (both engines).

- **F4 — `int()`/`float()`/`bool()` accepted an aggregate arg (List/Map/Set/tuple) at check, faulted at
  runtime — FIXED.** Check-OK-then-run-fault: the scalar-cast domain is int/float/bool/str (`spec.md`);
  an aggregate is outside it and — unlike a `struct` (whose structural `Convert` witnessing is a
  documented deferral) — can never carry a conversion, so the runtime always faulted (`float() cannot
  convert List`). New `reject_aggregate_scalar_cast` (`src/checker/expr.rs`) rejects at check. `str`-of-
  aggregate (a display) still passes. Test: `scalar_cast_rejects_aggregate_arg`.

- **F2 (doc) — `Shared.update` lock semantics + reentrancy limit** were documented only under `RwShared`.
  Added the note at `Shared.update` itself (`docs/stdlib.md`): `update(f)` runs under the box's exclusive
  write lock (atomic RMW — the reason it exists over `get`-then-`set`), and re-touching the **same** box
  inside `f` self-deadlocks — on M:N it **hangs** (no `deadlock` diagnostic; the channel-deadlock detector
  doesn't cover a mutex self-deadlock), and on the `--serial` oracle it **silently loses the inner write**
  (no real lock). So a same-box-reentrant `update` is a `--serial` ≠ M:N masker; documented, not chased.

- **F3 — generator reach-gate over-gates; docs contradict it.** *(✅ FULLY RESOLVED 2026-07-21 by
  backlog item B — the reach-gate is now DELETED, not retained. A module-global generator crosses BY
  VALUE like a frame-local one, so there is no gate left to over-fire and the doc-contradiction is moot.
  The historical write-up below — Path C landing + the `7b73e7c` judge-phase Poison-restore — is kept as
  the record of the intermediate state; the "retained belt-and-suspenders" and "remaining open follow-up"
  it describes no longer apply.)* Any spawned task that
  makes a call (`spawn: ch.send(99)`) or captures a **module-global** generator **faults** (`a generator
  cannot be sent across tasks`) whenever ANY module-global generator exists — even though the task never
  touches it. Both engines identical → **no soundness/parity bug**. But `docs/concurrency.md` +
  `docs/spec.md` claim "an untouched generator global does **not** fault," which is false for essentially
  every realistic task (the reach analysis conservatively treats any call as maybe-reaching). **Why held:**
  the memory note `generator-airlock-option-b-reach-gate` accepts over-gating deliberately — tightening the
  reach analysis to accept the repro risks an unsafe *under*-gate (a live generator, holding VM frames,
  crossing the airlock onto another OS thread = memory-safety/parity divergence), the exact hazard the
  over-approximation avoids.

  **F3 path C — LANDED (a LOCAL live generator is now sendable BY VALUE).** Instead of tightening the
  reach-gate (the risky, under-gate-prone direction), the airlock VALUE serializer (`to_wire`/`from_wire`
  only) now serializes a **frame-local** generator by value and rebuilds an **independent deep copy** on
  the receiver: `proto` (shared `Arc<Program>`), backing closure, and the parked operand-stack/args, each
  parked slot wired recursively so a **non-sendable parked slot rejects AT SERIALIZE TIME** (safer-in-
  direction — a slot check can only over-reject, never under-gate). A suspension **inside a `recover:`**
  (a live handler stack) now ALSO crosses by value (backlog item 3 arm b, 2026-07-21 — handlers are pure
  plain-data). The remaining rejected shapes are **checker-unreachable** and kept as defensive guards
  only: a suspension **with a pending `defer`** (`defer` is banned in a generator) and a **multi-frame**
  suspension (`yield` fires only in the generator's own body frame).
  `to_snap`'s module-global path stays `SnapValue::Poison` for generators, so the F1 shared-ref
  contract holds and a module-global generator still nil-replays + reach-gates. **Judge-phase fix
  (commit `7b73e7c`, applied during the main-loop review of the auto-task branch — the auto-task panel
  had DISMISSED this as unobservable):** making `to_wire` *succeed* for a sendable generator silently
  broke `to_snap`, because `to_snap`'s wire **fast path** (`if let Ok(w)=to_wire(v) && !w.has_handle()`)
  then caught a sendable module-global generator BY VALUE and returned `SnapValue::Wire`, bypassing the
  mandated `Obj::Generator => Poison` arm and eroding the Option-B defense-in-depth net (a reach-gate
  MISS would flip from an obvious Nil-replay to a silent serial-shared-vs-M:N-copy divergence). The fast
  path now excludes any generator-embedding value (`&& !self.value_embeds_generator(v, depth)`) so it
  falls through to the Poison arm — restoring "a module-global generator snapshots inert" while leaving
  the LOCAL `to_wire`/`from_wire` crossing feature intact. Not observably regression-testable (it is the
  backstop FOR a gate hole); guarded by the full suite + the ~15 unchanged reach-gate tests, and 3
  now-false airlock doc-comments were de-staled in the same commit. Touched: `src/vm/wire.rs`
  (`WireValue::Generator` + `WireGenState` + `WireCallFrame` + `has_handle`), `src/vm/sched.rs`
  (`to_wire`/`from_wire` arms + the `to_snap` fast-path generator guard), `src/vm/core.rs`
  (`collect_core_gcrefs`), `src/vm/stmt.rs` (`display_wire`). The reach-gate
  (`check_task_generator_reach`) is **retained** (now redundant belt-and-suspenders); its over-gate +
  doc-contradiction cleanup is the remaining open F3 follow-up.

## Session log — 2026-07-18 (8-byte `Value` shipped — one perf item BACKLOGGED)

The 8-byte `Value` milestone landed (int-favoring pointer-tag; commits `6c67eb9`/`fa3c014`, merge context
in `PROGRESS.md`, numbers in `docs/benchmarks.md`). It also surfaced + fixed a pre-existing soundness bug
(int `==` was lossy `as_f64` above 2^53 → now exact i64, `ccbd3c4`). One planned sub-task was deferred:

- **Float-constant interning — DEFERRED (backlog).** With 8-byte `Value`, every non-inline `f64` boxes
  into an `Obj::FloatBox` heap slot, so a float literal in a hot loop allocates one box per iteration.
  The mitigation (plan Task 5): intern compile-time float constants into one `FloatBox` at load, mirroring
  the existing runtime `str_intern` cache (`src/vm/exec.rs:87`, ctx-swapped per fiber at `exec.rs:185`) —
  add `Vm::intern_float(f64) -> Value` keyed on `f.to_bits()` and route the float-literal load opcode
  through it. **Why deferred:** the bench set (`benches/run.chz`) is int-heavy and showed **no float
  regression** — `str` flat, all others improved — so the churn cost is currently unproven (defer on
  VERIFIED cost, not speculation). **Revisit trigger:** a float-heavy workload (tight numeric loop over
  `f64` literals/results) where `Heap::live_bytes()` / GC frequency shows the per-iteration FloatBox churn
  actually costs. A heavier follow-on (Ruby-style flonum: inline common-magnitude `f64`, box only the rest)
  is the bigger lever if interning alone isn't enough — see design §2.

## Session log — 2026-07-18 (bug-hunt: 5 findings — 4 fixed, 1 backlogged)

Five-domain adversarial bug-hunt (airlock, cancel/defer, channel/nursery, checker⊋compiler, stdlib).
The checker⊋compiler int→float surface came back **clean** (5 prior waves + `88837d8` hardened it).
Five findings survived re-verification on the real binary, both engines:
- **A/B6** — `?` in a nil-returning fn silently swallowed the Err/None — **FIXED** (see B6 below).
- **C** — recursive-local-fn airlock misleading error — **FIXED** (diagnostic; see below).
- **D+str** — float formatting was Rust-style, not Python — **FIXED**: `{:e}`/`{:E}` (default precision 6,
  signed 2-digit exponent) + `str`/`print`/`json` scientific notation (CPython repr thresholds: sci when
  exp `< -4` or `>= 16`). One shared exponent-normalize helper; matches CPython exactly, both engines.
- **E — derived `std.cancel` token `done()` fired ~1ms BEFORE its deadline** (Go-context invariant break:
  a task woken by `done()` read `cancelled()==false`/`reason()==None`) — **FIXED**. `Token.derive`
  (`std/cancel.chz`) computed the child timer's remaining-ms with `int()` truncation toward zero; the
  `+ 1` ms margin keeps `done()` at-or-after the absolute deadline. Parity-preserving (both engines were
  wrong the same way → oracle-blind). Regression: `derived_cancel_token_done_implies_cancelled_runtime`.
- **B — nested `Option`/enum `match` false "non-exhaustive"** — **BACKLOGGED** (won't-fix pre-freeze):
  `match Some(Some(v)) / Some(None) / None` is exhaustive but the checker reports "missing Some". Root:
  `check_exhaustive` (`src/checker/pattern.rs`) marks a variant covered only by an *irrefutable* arm; it
  does not compute that `Some(Some(_))` + `Some(None)` recursively exhaust `Some`. It is an **over-reject**
  (safe direction — never accepts a truly non-exhaustive match; workaround is a `_` arm). A proper fix is
  a recursive-usefulness algorithm with real false-*accept* risk — deliberately not attempted right before
  the JIT freeze.

## Session log — 2026-07-18 (bug-hunt: recursive-local-fn airlock diagnostic)

**RESOLVED (diagnostic only — full support stays DEFERRED past the JIT freeze):** a nested (local)
recursive `fn` crossing the airlock (`spawn:` block, `spawn f()` callee, `spawn f(g)` arg,
`Channel[fn].send`) used to fault with the misleading `maximum structural depth (10000) exceeded (cyclic
data structure?)` — there is no cyclic *data*, just the letrec self-cell making the closure's capture
graph self-referential (`Closure h -> Cell -> h`), which tripped the generic depth guard. The two
closure-serialization arms (`to_wire_depth` / `to_snap_depth` in `src/vm/sched.rs`) now scan the crossing
closure's capture graph for its own handle (new `graph_reaches_handle`, sibling of the Task-2b
`graph_embeds_ref_depth`) and raise a clear, **recoverable**, byte-identical-on-both-engines error: `a
recursive local fn cannot be sent across a task boundary — hoist it to module scope (a module-global
recursive fn is sendable)`. The fix is in the message: a module-global recursive `fn` crosses as a plain
`Func` (no capture) and IS sendable. **Actual recursive-local-fn sendability remains deferred** (a risky
VM change post-JIT-freeze). Accepted ceiling: a genuine data cycle whose loop passes *through* a live
closure would now report the recursive-fn message instead of the depth message — pathological/rare, not
chased (`examples/cycle_guard.chz` / `airlock_cycle.chz` are pure data, unaffected).

## Bug found by the 2026-07-18 bug-hunt — FIXED

### B6. `?` in a nil-returning fn silently SWALLOWS the propagated `Err`/`None` — check-OK-then-data-loss — **FIXED (found 2026-07-18, fixed 2026-07-18)**

`infer_try` (`src/checker/pattern.rs`) accepted `?` whenever `current_ret == Ty::Nil` — but `current_ret`
is `Nil` for BOTH module top-level (legit — the runtime unwinds the unhandled `Err`/`None` at the program
boundary) AND a nil-returning fn body (the bug — the propagated `Err`/`None` was dropped on the floor, so
a `safe_div(..)?` in a `fn main():` type-checked yet lost its error). Closures already rejected this
correctly (they get `current_ret == Unknown`, hitting the rejecting arm), so the rule was inconsistent
across callable kinds.

**Fix:** added one checker signal `in_fn_body: bool` (false at module top-level, true inside any
fn/closure body), saved/restored 1:1 beside every `current_ret` `mem::replace` (`check_fn_body`,
`infer_fn_ret`, closure-infer) and reset false in `begin_module`. The two `Ty::Nil => {}` acceptance
arms in `infer_try` are now gated `Ty::Nil if !self.in_fn_body => {}`; inside a fn body they fall through
to the existing reject arm (`'?' used in a function that returns nil, not Result or Option`). No `fn main`
exception — a function must return `Result`/`Option` to use `?`.

**Runner symmetry:** `Vm::invoke_entrypoint` (`src/vm/exec.rs`) discarded a manifest `module:function`
entry fn's return value; it now routes the return through `top_level_error`, so an entry fn returning
`Err`/`None` surfaces as `unhandled error: <msg>` (rc=1) — symmetric with the unhandled-top-level rule,
and letting a project entrypoint legitimately be `-> T!` and use `?`. Both engines route through
`invoke_entrypoint`, so the one edit covers serial + M:N.

Migrated the two shipped examples that used `?` in a nil `main`/callee (output-identical, source-structure
only): `examples/hello.chz` (`fn main() -> int!` + `return Ok(0)`), `examples/socket_timeout.chz`
(`fn read_client(..) -> int!` + `return Ok(0)`). `recover.chz`/`edge_cases.chz` were false positives —
their `?` sits inside a `recover:` block (`recover_depth > 0` short-circuits before the Nil gate).

## Session log — 2026-07-16 (stdlib gap-fill, waves 1–3: six gaps shipped)

One session, six gaps off this backlog's *ranked stdlib* list, run as three concurrent-pair waves
(auto-task → fix confirmed bugs → serial merge → post-merge-gate → worktree cleanup). All merged to
`main`, verified end-to-end on the real binary, both engines; final HEAD `6bd2348`, lib suite 3566 green.

**SHIPPED (each de-staled in its own section above):**
- **§1** — `std.string` ergonomics: `capitalize`/`title`/`swapcase`/`find(s,sub,from_index)`/`split(s,sep,maxsplit)`/`rsplit`/`split_whitespace` (pure-Chezzi). Confirmed-bug fixed pre-merge: `find` negative `from_index` clamped to 0 instead of Python's `len+from_index`.
- **§9** — `datetime.parse_iso8601` (pure-Chezzi); `datetime` is no longer write-only. Fixed pre-merge: an unbounded year overflowed i64 → a *fault*; now a clean `Err` (≤9-digit guard).
- **§9** — `std.duration` (new pure-Chezzi module): Go-like first-class `Duration` (int-ms), unit constructors/accessors/arithmetic, `to_string`/`parse` round-trip, `since`/`sleep`. Sub-ms → clean `Err`; magnitude bounded (≤12-digit int) to keep an oversized parse a clean `Err` not an i64 fault.
- **§10** — `std.flag` (new pure-Chezzi module). Fixed pre-merge: bool `=`-form only took `true`/`false`; now the full Go `strconv.ParseBool` set.
- **§7** — `encoding.query_decode` + `url_parse` (**Rust native** — see the correction below). 0 charges.
- **§10** — `std.log` (new pure-Chezzi module, leveled, stderr-default, deterministic `format_line`). 0 charges.
- **§5** — `std.math` number fns: `gcd`/`lcm`/`sign`/`trunc`/`hypot`/`cbrt`/`factorial`/`comb`/`perm`/`parse_int_base` + `inf`/`nan` (**Rust native**). Fixed pre-merge: a `comb` i128-intermediate overflow and `parse_int_base` accepting an embedded/non-leading sign (`"+-5"`/`"0x-5"` → `Ok`).

**SEAM LESSON (bit two runs — record so it isn't re-learned):** `encoding`, `math`, `regex`, `io`,
`crypto` are **file-backed NATIVE** modules — every member is a bodyless `native fn` decl in
`std/<m>.chz` implemented in `src/native/<m>.rs`; a free pure-Chezzi fn added there is **dead code**
(never harvested/compiled). Additions to those modules are Rust. Pure-Chezzi modules (`string`, `cmp`,
`datetime`, `flag`, `log`) take plain `.chz` fns + one `include_str!` line in `src/resolver/std_embed.rs`
(guarded by `embedded_std_table_matches_disk`). Check which kind a module is before scoping a gap-fill.

**STILL OPEN on the ranked list after this session:** §2 (List/iter ergonomics — List wave-1 + wave-2
SHIPPED; still open: `iter.min`/`max`, `group_by`/`partition`/`flat_map`, Map/Set ergonomics — later
waves), §3 (lazy itertools — SHIPPED), §4 (IO
seek), §5 (`divmod` SHIPPED as a bodied Chezzi fn — no `NativeRet::Tuple` needed; decimal/bigint hard wall), §6 (os/system — `isatty` +
`setenv`/`chdir`/`getpid`/`environ`/`platform`/`hostname`/`home_dir`/`temp_dir` SHIPPED; signals/atexit
+ metadata-reader still open), §7
(bcrypt/argon2, gzip; secure-random/token + sha1/sha512/hmac_sha256 + CSV SHIPPED), §8 (net depth), §9
(`strptime` — Go-like `Duration` SHIPPED as `std.duration`), §10 (`std.db`, config formats; `bisect` + `memoize` SHIPPED), §11 (`std.process` `Child`).

## Session log — 2026-07-14 → 2026-07-15 (Tier-0 + R1 + the cancel-teardown cascade)

One session, driven off this backlog. Each item links to its full entry.

**RESOLVED (merged to `main`, verified end-to-end on the real binary, both engines):**
- **T1** — an installed `chezzi` couldn't find its own stdlib; `std/` is now `include_str!`'d into the
  binary (`cda71b5`/`56ec7a7`). See [T1](#t1-installing-chezzi-produces-a-binary-that-cant-find-its-own-stdlib--fixed).
- **T2** — `repl` was advertised in `--help`/`spec.md`/`CLAUDE.md` but never existed; de-advertised
  (`e2e7707`). See [T2](#t2-chezzi-repl-is-a-stub-that-errors--while---help-advertises-it--fixed-de-advertised).
- **B1** — `Socket.read` silently corrupted data via `from_utf8_lossy`. First **mitigated** (carry
  split codepoints, `Err` on binary — `95f37ef`/`6477e45`/`d784031`/`26030f4`), then **fixed honestly**
  by R1's `Socket.read_bytes`/`write_bytes`. See [B1](#b1-socketread-silently-corrupts-data-from_utf8_lossy--p0--fixed-2026-07-14-r1).
- **R1** — the native seam couldn't carry `bytes`; added `NativeRet::Bytes` + `Host::arg_bytes` +
  `NativeArg::Bytes` (the offload-path piece the entry omitted) and wired consumers: binary file IO,
  binary sockets, `sha256`/base64 of bytes (`f09ede0`/`eb300bb`/`0b23703`). See [R1](#r1-the-native-seam-cannot-carry-bytes--done-2026-07-14).
- **N1..N9 — the cancel-teardown cascade.** R1's post-merge gate flushed out a family of pre-existing
  concurrency bugs around `defer`-on-cancel. The through-line: **`defer` is the language's only cleanup
  mechanism, and a cancelled task was silently skipping it.** Fixed by adopting **cancellation points**
  (a deliberate semantics change — cancel is delivered at loop back-edges + blocking ops, not every
  instruction), so a *registered* `defer` now runs on a cancelled task deterministically on **both**
  engines. Landed across `4ac04ce`→`e70fb5f`. Sub-fixes N4/N6/N6b–N6h each have their own entry below.
  Suite grew 3450 → 3485 tests.

**PROCESS NOTE (recorded so it isn't repeated):** the cancel work took **five** auto-task rounds, two of
which I merged or nearly-merged on a green result from a repro I'd designed to pass — a channel-token that
*sequenced* the fault and hid the race. The adversarial panel was right twice where my own verification
was too easy on itself. Lesson: **a green result from a test you wrote to pass is not evidence of a race
fix** — measure the natural (unsequenced) shape, ≥200 runs under CPU load. Two auto-task runs also leaked
CPU load-generators (`yes`, spin loops) that burned cores for hours; reap anything you spawn.

**STILL OPEN after this session (ranked):**
- [N8](#n8---serial-hangs-on-a-cpu-bound-sibling--cooperative-engine-never-preempts-it--open) / [N9](#n9-a-cancelled-tasks-output-line-set-differs-between-engines--inherent-open)
  — **DOCUMENTED KNOWN-LIMIT, won't-fix** (2026-07-15): `--serial` HANGS on a CPU-bound sibling / a
  cancelled task's line set differs by engine. `--serial` is only the parity **oracle** for
  bug-finding, never the user runtime; `--threads=1` gives safe single-thread execution (OS-thread M:N,
  kernel preempts — 0/15 hangs) and makes a cooperative-scheduler time-slicer unnecessary. Recorded in
  `docs/concurrency.md` §"Cooperative contract (by design)". Reopen only if `--serial` ever becomes a
  shipped user runtime.
- [N6g / C5](#n6g--open-c5-family-a-defer-that-recvs-from-a-live-sibling-cannot-park-on---serial) — a
  `defer` that must **park** (recv from a live sibling) can't, on `--serial` — needs a VM-driven defer
  drain, its own milestone.
- [N1](#n1-a-last-print-into-a-just-closed-pipe-exits-0-or-1-nondeterministically--fixed-2026-07-15) **(FIXED
  2026-07-15)** — a last `print` into a just-closed pipe exited 0-or-1 nondeterministically; now
  deterministically non-zero (Python-matching) via a post-`flush_stream()` `out_dead_reason()` check in `cmd_run`.
- [N2](#n2-socketwriteaccept-still-restart-their-timeout-budget-on-every-park--fixed-2026-07-15) **(FIXED)**,
  [N3](#n3-two-cosmetic-b1-leftovers) — small B1/socket residuals: N2 + N3(a) fixed 2026-07-15; N3(b) stays as-is by design.
- [N5](#n5-a-genuine-deadlock-tears-tasks-down-without-running-their-defers--closed-2026-08-06-not-a-bug-every-ancestor-does-the-same-or-worse) **(CLOSED, not a bug)** — a *genuine*
  deadlock skips defers, which is what Go's deadlock `fatal error` does and better than CPython (hangs).
- Backlog headliners: **R2** (Writer/file handles) **DONE 2026-07-15** + **R2b** (Reader/file handles)
  **DONE 2026-07-15**; **R3** (package manager) still
  open — see their sections. (**R4** runtime type tags and **L3** error-handling machinery were reviewed
  2026-07-15 and marked **won't-do**; **L1** `Result`/`Option` methods deprioritized — we're not
  imitating Rust's method surface.)

## Bugs found by the 2026-07-14 audit — FIX, do not backlog

### B3. Mutating a captured MODULE-GLOBAL aggregate inside a task diverges serial (shared) vs M:N (lost) — serial≠M:N soundness — **CLOSED by construction: serial now snapshots module globals per task, matching M:N (2026-07-21)**
A task (`spawn`/`parallel:`) that **mutates a captured module-global mutable aggregate** in place
(`List`/`Map`/`Set`/`struct` — `.push`, `m[k]=v`, `s.add`, `s.field=x`, nested) diverges between engines:
on **`--serial`** the module global is shared by reference so the mutation **leaks** (visible to the parent
+ siblings); on **M:N** the per-task module-globals snapshot deep-copies it, so the mutation is **silently
lost** (invisible to everyone — the `Shared.get()`-mutate-a-throwaway gotcha, but implicit). A **silent
value divergence** that breaks the `serial == M:N` invariant the parity oracle rests on.

Minimal repro (deterministic, 5/5) — `xs` is a **top-level** binding (a module global):
```chezzi
xs := [1, 2, 3]
parallel:
    spawn:
        xs.push(99)
print(xs.len())      # --serial: 4 (leaked)   |   M:N (default): 3 (mutation lost)
```
Also reproduces with `Map`/`Set`/struct-field, and independent of spawn form (block, `spawn f()` callee,
closure-indirect `w := fn(): xs.push(..)`, and a closure reached through a captured struct field all leak).

**The real trigger is the MODULE-GLOBAL-ness, not the spawn form** (corrected 2026-07-17 after a
mis-scoping — an airlock subagent flagged the callee/closure forms leaking too; the common factor is that
those repros bound `xs` at top level):
- ❌ diverges: a **module-global** (top-level-bound) mutable aggregate mutated in a task.
- ✅ isolates on BOTH engines: a **function-local** aggregate — direct block capture, `spawn f(xs)` arg-pass,
  and closure-indirect capture ALL deep-copy correctly on both engines when `xs` is a `fn`-local (verified:
  same repro inside `fn main():` gives serial=3 / M:N=3). Also fine: scalar reassignment (a cell/copy);
  `Channel.send` of an aggregate; `Shared.get()` snapshot; `Executor.submit` (the
  [#3](#3-executorsubmit-coop-vs-mn-capture-sharing-divergence--resolved-2026-07-11) fix holds).

**Fix (landed 2026-07-21) — approach (b): the SERIAL engine now snapshots module globals per spawned
task, exactly as M:N already did.** The 2026-07-17 checker fix (a) — a frozen-module-global rule that
REJECTED the mutation — was an interprocedural, brittle, leaky patch (its residuals (A)/(C)/(D) below were
the cases the transitive scan could not follow). It is **deleted**. The root cause was that a cooperative
child aliased the shell's real `module_objs` while an M:N fiber installed its own snapshot; now
`join_nursery`'s serial branch reuses the SAME memoized `ensure_snapshot` M:N uses (NOT a fresh
per-nursery `snapshot_modules()`) and `prepare_serial_child` deep-copies the module globals into each
child's OWN `module_objs` view **in the shared heap** (reusing the exact M:N `to_snap` lowering + eager
`fault_module`), swapped in/out per fiber by `swap_ctx`. So `counter.bump()` / `xs.push(99)` / `g = g + 1`
in a task all mutate the task's **private copy** on BOTH engines — `serial == M:N` **by construction**,
nothing to reject. The memo matters: M:N snapshots module globals **once at the first nursery** and every
worker + nested nursery reuses that frozen `Arc` (invalidated nowhere), so a global mutated after the first
nursery — by sequential parent code between nurseries, or by a task before it opens a nested `parallel:` —
is invisible to later tasks; serial freezes at the same instant from the same memo, else it would read the
live post-mutation copy and diverge.
> **RETIRED 2026-07-25 (W6-2).** That frozen-forever memo was the bug: a global not yet initialized when it
> was built replayed as `nil`. Each task now snapshots FRESH, pinned at its own `spawn`, at every depth, so
> both engines still snapshot at the same program point. The per-task deep copy / isolation described above
> is unchanged. See `### W6-2` in the 2026-07-25 log.

Every task-entry path snapshots — not just the nursery. `Executor.submit` closures also mutate their OWN
module-global copy on both engines: the cooperative `Executor.shutdown` inline drain
(`src/vm/netio.rs`) runs each submitted task under a fresh per-task child module view
(`with_serial_child_modules`, the serial analogue of M:N's `drain_executor_on_pool` →
`prepare_worker_from_wire` → `install_snapshot`), reusing the same memoized `ensure_snapshot`. Before this
the serial drain aliased the shell globals — an in-place `xs.push(99)` or a free-fn-callee `bump()` reassign
inside a submitted closure leaked on serial while M:N isolated. Now both isolate (proved by
`executor_submit_module_global_inplace_mutation_isolates_parity` and
`executor_submit_module_global_callee_reassign_isolates_parity`).

The mutation does **not** propagate to the parent on either engine (it never did on the shipping M:N
engine — this makes serial agree). The escape hatch for genuinely-shared cross-task state is unchanged:
`Shared[T]` / `RwShared[T]` / `Atomic[T]` / `Channel[T]` cross by shared `Arc` core (via `to_snap`), so a
task-side `a.add(1)` on a module-global `Atomic` IS visible to the parent — through a nursery
(`atomic_incremented_in_task_visible_to_parent_parity`) AND through an `Executor.submit`
(`executor_submit_atomic_visible_to_parent_parity`).

Deleted (were compensating for the divergence): `check_spawn_global_mutation` (the transitive scan) + its
free-fn helpers, the method-mutation gate (`infer_method_call`), the index/field-assign gate
(`check_assign`), and the reassign gate — plus their `rejects()` checker tests. KEPT: the local-capture
sendability gate (`is_local_capture(name) && !sendable(ty)`) and `to_snap`'s non-sendable arms (Poison for a
frame-holding generator, Arc-share for handles). The generator reach-gate is left in place this run
(redundant-but-harmless; a separate follow-up).

**Residuals (A)/(C)/(D) — RESOLVED by construction.** The forms the checker scan could not follow
(closure-valued spawn root, callee-form method-mutation, task-local alias `local := xs; local.push(..)`) now
just isolate like every other form — the task mutates its own copy regardless of how the mutation is
reached, so there is nothing to statically follow. Regression net (all `src/vm/parity_tests.rs`, serial ==
M:N): `serial_module_global_method_call_mutation_isolates_parity` (A, cross-module fn call),
`serial_module_global_spawned_callee_mutation_isolates_parity` (C), `serial_module_global_task_local_alias_isolates_parity`
(D), `serial_module_global_direct_mutation_forms_isolate_parity` (list/map/struct/set/bytearray/reassign),
`nested_serial_spawn_module_global_isolates_parity`, and `channel_park_keeps_module_snapshot_parity`.
The freeze timing (memoized snapshot, not fresh-per-nursery) is pinned by
`nested_serial_spawn_mutation_before_nested_reads_frozen_parity` (a task mutates then opens a nested
nursery — grandchild reads the frozen pre-mutation value) and
`sequential_mutation_between_nurseries_reads_frozen_parity` (a global mutated by sequential code between
two nurseries stays frozen for the second nursery's task) — both were serial≠M:N under a fresh-per-nursery
snapshot, now equal.

### B4. An `Executor`-task uncaught error prints more backtrace frames on `--serial` than M:N — cosmetic serial≠M:N — **FIXED (found 2026-07-17, fixed 2026-07-17)**
**Fix:** serial dropped the inline task's callee frames to match M:N (and a plain nursery-task panic).
Serial's `Executor.shutdown` drains each submitted task INLINE on the entry `Vm`, so the task's callee
frames were captured into `fault_trace` while intact and survived to the top; M:N runs each task on an
isolated worker `Vm` and discards that worker's `fault_trace`. `src/vm/netio.rs` (serial shutdown drain
loop) now snapshots any pre-existing `fault_trace`/`fault_trace_depth`, gives the inline task a clean
capture slate, and **restores that snapshot** after the task runs — dropping ONLY the inline task's own
callee frames, never a superseding outer fault. On the common path the snapshot is empty, so the
propagated fault re-captures at the shutdown call site in the enclosing `run_until` — both engines print
just `at main`. Three cases converge (verified both engines):
- **explicit `ex.shutdown()`** → both `at main`;
- **`defer ex.shutdown()` while `main` is unwinding** (the outer fault is already captured) → both
  `at main` — the snapshot/restore is what preserves the outer `[main]` here instead of nuking it to
  `[]` (the initial `= None` clear got this wrong: serial `[]` vs M:N `[main]`; caught in review);
- **implicit end-of-program `drain_live_executors`** (no `ex.shutdown()`) → the executor is reaped
  *after* `main` returned, so there is no enclosing `run_until` to re-capture at: **both engines print
  an EMPTY trace** (parity holds — both `[]`, not `at main`).
Message/location/rc unchanged on all three. Two-engine test:
`executor_task_fault_trace_matches_on_both_engines` (src/vm/parity_tests.rs) covers all three + the
non-Executor nursery neighbor.

<details><summary>Original report</summary>
An uncaught runtime error thrown from an `Executor.submit(...)` closure prints a **full backtrace on
`--serial`** (`at boom` / `at <closure>` / `at main`) but a **truncated one on M:N** (just `at main`) —
**same error message, same source location, same exit code (1)**, only the intermediate call frames differ.
Deterministic (5/5 each engine). M:N is internally consistent (a plain nursery-task panic drops to `at main`
on both engines); the outlier is that `--serial`'s Executor path uniquely preserves the submitted closure's
callee frames while M:N discards the submitted fiber's `fault_trace` when re-raising through `shutdown`.
Cosmetic — the load-bearing parts (message/location/rc) match — so it is **not** a soundness bug, but it is
a real serial≠M:N observable-output divergence the parity suite doesn't cover. Fix: have the M:N Executor
error-propagation path carry the submitted fiber's frames (or, symmetrically, have serial drop them) so the
two agree. Low priority.
</details>

### B5. M:N spuriously DEADLOCKS an uncontended cross-nursery send→recv (a nested-nursery `send` doesn't wake an outer parked receiver) — `--serial` works — **FIXED (found 2026-07-17, fixed 2026-07-17)**

> **Fix (child→parent eager wake routing).** The residual was narrower than the OPEN note guessed: not
> the shared global `MnSched` (lazy nested nurseries already share it), but an **EAGER** nested nursery's
> **private** `MnSched` (`activate_eager_nursery` — entered when a `parallel:` runs inside a live worker
> fiber on a ≥2-core box, for per-connection liveness). Its `send_wake`/`close_wake` only scanned its own
> park set, so a `send` inside the eager body queued the value into the shared `ChannelCore` but never woke
> a receiver parked on the PARENT sched → false `deadlock`. Added `MnSched::parent_wake: Option<Arc<MnSched>>`
> (`None` for every ordinary sched; set on an eager sched to the activating parent sched via
> `self.mn.or(mn_enlist_sched)`); `send_wake`/`close_wake` now walk that chain (strictly UPWARD — no cycle,
> no ABBA, each ancestor woken under its own lock after the eager core guard is dropped) and requeue the
> parent's parked receiver onto its home sched. Value already in the shared queue → the woken receiver pops
> it (no double-consume); over-wake (empty queue → re-park) is the tolerated pattern. `is_deadlocked` is
> UNCHANGED — a genuine no-sender quiesce still faults. Golden
> `parallel_cross_nursery_nested_send_to_outer_recv.chz` (serial==M:N=="receiver got 1"; 30× on M:N under
> CPU load, 0 flakes) + `parity_tests.rs` parity + three guards (genuine-deadlock-still-faults,
> real-fault-reports-real-error, parent→child-residual-never-panics). **Residual (documented, pinned):**
> parent→child (receiver parked INSIDE an eager body, sender in an ancestor — `parent_wake` points UP
> only) and sibling-eager→sibling-eager remain timing-divergent (complete-or-deadlock-fault cleanly); a
> descendant walk / VM-global registry would be a larger pre-freeze change (out of scope). See
> `docs/cross-nursery-flat-scheduler.md` (eager bullet + §3).

On the **default M:N engine**, a `send` issued from inside a **nested** (child) `parallel:` does not wake a
single receiver parked on that channel in an **outer/ancestor** nursery — so M:N declares a **false
`deadlock`** and faults, while `--serial` runs the program correctly. A real correctness bug in the
**primary engine** on a legitimate structured-concurrency fan-out shape (a nested nursery produces into a
channel an outer sibling consumes), not just a parity artifact.

Minimal repro (deterministic — `--serial` 6/6 `rc=0`, M:N 8/8 `deadlock`):
```chezzi
import std.concurrency
fn main():
    ready := Channel[int]()
    parallel:
        spawn:
            parallel:                 # nested nursery
                spawn:
                    ready.send(1)     # send from a nested-nursery grandchild
        spawn:
            v := ready.recv()         # receiver parks in the OUTER nursery (inside a spawn:)
            print("receiver got {v}")
main()
# --serial: "receiver got 1"  rc=0   |   M:N: runtime error … deadlock …  rc=1
```
It is purely a **parked-receiver wake** gap: if the receiver is delayed so the `send` lands first, M:N
reads the buffered value fine (the value is never lost) — the failure is only when the receiver parks on
the empty channel *before* the nested-nursery send, and the cross-scope wake isn't routed.

**Root cause (already documented, but as RESOLVED):** `docs/cross-nursery-flat-scheduler.md:150-155` —
`MnSched.parked` is keyed per-nursery, so a `send`/`close` in another nursery *delivers the value but does
not wake across scheds* (`src/vm/mod.rs:5810`). That doc's banner claims this routing class is **"RESOLVED
under `--parallel`"** and that *"independent / normal multi-level nesting is fully supported … RUNS under
`--parallel` and matches the cooperative engine."* This repro contradicts both. It is **NOT** one of the
doc's enumerated allowed limits: it is **uncontended** (1 sender / 1 receiver — the allowed contended limit
is *2+ receivers racing one channel*), the receiver's `recv` is inside a `spawn:` (**not** the Case-B inline
outer-body recv), and there's no eager/per-connection nursery. So it's a **coverage gap in the flat-scheduler
fix**, not a divergent-by-design case. (Note the doc's limit #2 says the *cooperative* engine faults on this
class — the OPPOSITE of this shape, where `--serial` succeeds and M:N faults, because cooperative runs the
innermost sender-nursery to completion first so the value is buffered before the outer recv.)

**Also masks teardown diagnostics:** the same wake gap replaces a faulting nested-nursery sibling's real
error with this spurious `deadlock` — so fixing the wake also cleans up nested-nursery fault reporting.

Fix: route the parked-receiver wake across nursery scopes on M:N (the flat-scheduler design in §4 of that
doc — one flat runnable/park set keyed by `ChannelCore` ptr, nursery = join record). Add the missing golden:
`parallel_cross_nursery_nested_send_to_outer_recv.chz` (+ a serial==M:N parity assert). This is an M:N VM
change — the riskiest of the 2026-07-17 finds pre-JIT-freeze; scope carefully.

### B1. `Socket.read` silently CORRUPTS data (`from_utf8_lossy`) — P0 — **FIXED (2026-07-14, R1)**
`src/vm/netio.rs:315` and `:360` did `String::from_utf8_lossy(&buf)`, and `std/net.chz` types the
method `read(self, n: int, ...) -> Result[str]`. So the socket seam **had to** lossily decode. Two
failures, both silent — no `Err`, no fault, just wrong data:
1. **Any binary payload** (TLS, an image, protobuf, a gzip body) becomes U+FFFD replacement chars.
2. **Even pure UTF-8 text** is mangled when a multibyte codepoint straddles a `read(n)` chunk boundary
   — i.e. the ordinary "read in a loop" idiom. VERIFIED end-to-end (`--parallel`, localhost TCP,
   sending `"héllo"`, reading 1 byte at a time):
   ```
   expected   : héllo
   reassembled: h��llo      # equal? false
   ```
This is the same family as the false-EOF and the swallowed exit status: **the runtime lies to the
program.** It is worse than those, because it corrupts *data* rather than control flow, and `std.net`
is documented as working.

**MITIGATION LANDED (2026-07-14).** Both lossy sites now route through one guard,
`Vm::decode_carry` (`src/vm/netio.rs`), and `from_utf8_lossy` is gone from the socket path.
The two failure modes are now separated, exactly as `Utf8Error` separates them:
- **Split codepoint (`error_len() == None`) — case 2 is FIXED, not merely reported.** The incomplete
  ≤3-byte tail is retained on the `SocketCore` and prepended to the next read, so a byte-at-a-time read
  of valid text reassembles **byte-exactly**. Contract: `n` bounds the NEW bytes off the fd, so a
  `read(n)` may return up to `n + 3` bytes; a read whose chunk holds no complete codepoint re-reads
  (never `Ok("")` — that is the EOF sentinel), so it may block past its first fd read. `timeout_ms` bounds
  the WHOLE call (the deadline is latched on the fiber — `Vm::poll_deadline` — so re-parking to finish a
  codepoint does not re-arm the budget — on the in-callback demote path too), and the carry survives a
  timeout `Err`. Blocking for the rest of a character is the Go `bufio.Reader.ReadRune` / Python
  text-mode-socket contract. A poll-once `read(n, 0)` that took a partial codepoint says so —
  `Err("incomplete utf-8: …")`, not the `Err("timeout")` that means *nothing arrived*. `read(0)` is a
  no-op `Ok("")` (it never touches the fd, so it can neither spin nor fake an EOF) but still reports a
  closed socket, and the fd read + carry update are ONE critical section (carry lock outer), so two tasks
  sharing a socket decode in wire order.
- **Genuinely invalid bytes (`error_len() == Some(_)`) — i.e. a BINARY payload — case 1 is REPORTED, not
  supported:** `Err("invalid utf-8 on the socket: std.net read is str-only — binary payloads need
  Socket.read_bytes …")`. The error is **non-destructive and sticky**: the valid text that arrived before
  the bad byte is delivered first, the undecodable bytes stay carried on the socket, and every later read
  re-errs identically — so a caller that logs the `Err` and keeps reading (what a `Result` invites) cannot
  silently shred the stream. It must `close()`. (Swallowing the chunk would just be silent data loss
  wearing an `Err`.) An incomplete codepoint left when the peer closes is likewise
  `Err("invalid utf-8 at eof: …")`, never a silent drop.

**FIXED (2026-07-14) — R1 landed the honest fix.** `Socket.read_bytes(n[, timeout_ms]) -> Result[bytes]`
and `Socket.write_bytes(b[, timeout_ms]) -> Result[int]` (`src/vm/netio.rs`, declared in `std/net.chz`):
they never decode, so **binary sockets work byte-exactly**. `read_bytes(n)` returns AT MOST `n` bytes
(the natural byte contract — the str `read`'s `n` bounds only the NEW fd bytes, hence its `n + 3`), `Ok(b"")`
is the EOF sentinel, and it **drains the carry first** — so the undecodable bytes the str `read`'s sticky
`Err` refused to deliver are recovered here instead of forcing a `close()`. The str `read` keeps its
documented decode contract, unchanged (`read_bytes` is purely additive).
**What remains is not a defect:** the caller must pick the right method — a `str` seam cannot hand back
bytes that are not UTF-8, and it now says so and points at `read_bytes`.

### B2. `==` between disjoint types type-checks (a proposed tightening, not a clear bug)
`1 == "a"` compiles and evaluates to `false` (`src/checker/pattern.rs`, the `Eq | NotEq` arm returns
`Ty::Bool` without checking operand compatibility). Note the tension before "fixing" it: this is
**exactly Python's runtime behavior** (`1 == "a"` → `False`), so by the no-drift rule it is not a
divergence. But Chezzi is **statically typed**, and a comparison between provably disjoint types is
always a bug in user code — which is why mypy ships `--strict-equality` to reject it and Go/Rust make
it a compile error. Recommendation: reject at check time (a typed language should), and say so in the
docs as a deliberate, explained divergence from Python's runtime.

## Root causes — one change each, many gaps unblocked

These are the entries that were previously scattered as unrelated one-liners. Ranked by how much they
unblock.

### R1. The native seam cannot carry `bytes` — **DONE (2026-07-14)**
`bytes`/`bytearray` existed in the language, but `NativeRet` had no `Bytes` variant and `Host` no
`arg_bytes` (`src/native/mod.rs`), so **no native fn could accept or return them**. Landed as a seam
expansion (no new type, no heap obj, no GC/airlock work — they already shipped below the seam):
`NativeRet::Bytes` (lowered by `Vm::lower_native` to the immutable `Obj::Bytes`), a defaulted-to-error
`Host::arg_bytes` (on `VmHost`: `bytes`-only — a `bytearray` is not assignable to a `bytes` sink
(7b29552), so a built-up buffer is passed as `bytes(ba)`, the explicit copy CPython also makes), and
`NativeArg::Bytes` + `OffloadHost::arg_bytes` so a *blocking* bytes native still offloads to the dirty
pool instead of pinning a core worker (D5). `value_to_native_ret` gets no bytes arm on purpose (it fills
C's return register; a callback return is checker-restricted to C scalars).
Consumers wired, and the gaps that were filed separately as if each were its own feature:
- binary file read/write → **DONE**: `io.read_bytes(path) -> Result[bytes]` / `io.write_bytes(path, b) ->
  Result[nil]` (`read_file` decodes UTF-8, so it hard-failed on any binary file — it now errs with
  `use io.read_bytes for binary files`). Same 64 MB read cap; `write_bytes` uncapped, like `write_file`.
- arbitrary-bytes base64 round-trip → **DONE**: `encoding.base64_encode_bytes` / `base64_decode_bytes`.
  gzip/zlib → **still open** (a new dependency, not a seam gap).
- binary sockets → **DONE**: `Socket.read_bytes` / `write_bytes` — this is the fix for **B1** (above).
  A hand-rolled HTTP server can now accept an image.
- `sha256` of a file / hashing binary data → **DONE**: `crypto.sha256_bytes(b)` over `io.read_bytes(p)`.
- `std.request` binary fetch → **DONE (2026-07-15)**: `request.get_bytes(url, timeout_ms?) ->
  Result[bytes]` reads the body via `into_reader().read_to_end` → the same immutable `bytes` value
  `Socket.read_bytes`/`io.read_bytes` return, so an image/zip/pdf round-trips byte-exactly instead of
  going through `into_string()`'s `from_utf8_lossy` corruption. GET-only + body-only: a non-2xx status
  is an `Err` (a 404/500 error page can't pose as a successful download — `io.read_bytes` semantics),
  headers dropped; 64MB download cap mirrors `io.read_bytes`. The text `get`/`post` path is unchanged.

### R2. `Writer` / file-handle type — **DONE (2026-07-15)**
Landed a write-only `Writer` native handle in `std.io` (the `Socket` handle is the template): openers
`create` (truncate) / `append` (create-if-absent), stream handles `stdout()` / `stderr()` (routing
through the same `Vm::emit_out`/`emit_err` sink as `print`, never a raw fd), a `buffered(w, size = 8192)`
wrapper (the Go `bufio.NewWriter` escape hatch — one host/fd write per `flush`/buffer-full/`close`), and
methods `write`/`write_bytes`/`flush`/`close`. Sendable across the airlock like `Socket`; cross-task
write ordering to one shared handle is unspecified (Go's `bufio`-not-goroutine-safe rule). Runtime in
`src/vm/fileio.rs` (blocking-classified, no netpoller); type `Ty::Writer` gated by `import std.io`. So:
buffering is now **a value you hold**, not a global mode; `io.flush()` keeps its honest no-op meaning for
the process's unbuffered stdout while `buffered(...).flush()` is the real thing. `std.fs`'s
`fs.append(path, text)` whole-file appender is untouched (no collision — `std.io` owns the handle verbs).
**Deliberately out of scope (still open, separate IO §4 gaps):** seek / random-access. (Reader /
line-streaming of a large file — the write side's twin — landed as **R2b**, below.)
**Follow-up — promote `Writer` to a structural protocol (Go `io.Writer` parity).** As shipped, `Writer`
is a **sealed concrete native handle** (four `Backing` arms baked into the runtime), NOT an interface —
so a user cannot implement their own writer (a `StringWriter`, `TeeWriter`, byte-count/limit wrapper, or
test spy), which is one of the most-used Go patterns (`func(w io.Writer)` polymorphic over file /
buffer / socket / gzip). This is **mild north-star drift** (Go's `io.Writer` is an interface; Chezzi's
Go-analog is a structural protocol), not a bug — behavior is correct, the surface is just smaller. The
right end state: a `protocol Writer` (write/write_bytes/flush/close) that the native handles *satisfy*,
with `buffered(w: Writer)` polymorphic over the existential. **Cost, honestly:** the runtime is nearly
free (method dispatch already keys on the heap variant `Obj::Writer`, `call.rs:1006` — not the static
type, so an existential over a native handle dispatches unchanged); the work is checker-side —
(1) the **unproven seam**: a native opaque handle satisfying a protocol *existential* + dispatching has
no in-tree precedent (existentials today resolve over user structs + a `str→Error` intrinsic), so it
could be a small arm or a rabbit hole — **spike it before committing**; (2) rewiring the ~7 reserved-name
touch points (`Ty::Writer` → an internal concrete handle name, protocol `Writer` takes the name). **When
to do it: YAGNI until a second implementer exists** (a user custom writer, or the `Reader` twin's
symmetric design) — a protocol over a single native concrete family is ceremony with no payoff yet.
**Known ceiling (mapped in-tree):** the stream queue is **unbounded** (`src/vm/stream.rs:26-27`, a
`ponytail:` comment naming the same upgrade path) — a program printing faster than a stalled consumer
drains grows memory without limit. Deliberate (never pin a core worker), but it is a real ceiling;
bounded `sync_channel` is the upgrade. (Independent of R2 — buffering the *producer* does not bound the
*queue*.)

### R2b. `Reader` / read-only file handle — **DONE (2026-07-15)**
Landed the read twin of R2's `Writer` (same `Socket`/`Writer` handle template): a read-only `Reader`
native handle in `std.io`, opener `open(path)`, methods `read_line()` / `read_bytes(n)` / `close()`.
`read_line() -> Option[str]` streams one line at a time (trailing `\n`/`\r\n` stripped, `None` = EOF) —
matching the module-level `read_line()` shape (anti-drift); a mid-read I/O error or non-UTF-8 file is a
clean runtime fault pointing at `read_bytes` (an `Option` can't carry the error, mirroring `read_file`).
`read_bytes(n) -> Result[bytes]` is the binary + error-distinguishing escape hatch (at-most-n bytes,
empty = EOF, `Err` on closed/IO). `close() -> Result[nil]` idempotent (fd closes on `BufReader` drop —
no `Drop` impl needed, reads are flush-free). Sendable across the airlock like `Writer`; cross-task read
ordering to one shared handle is unspecified (two tasks race the file offset). Runtime in
`src/vm/fileio.rs` (blocking-classified, no netpoller — an inline blocking read pins an M:N worker on a
slow fifo, the same accepted ceiling `Writer.write` carries, `ponytail:` comment); type `Ty::Reader`
gated by `import std.io`. So a big file can now be read **line/chunk-by-chunk** instead of slurped whole.
Whole-file `read_file`/`read_bytes` (≤64 MB) untouched.
**DONE:** `lines() -> Iterator[str]` — the idiomatic method form of line-streaming (Python `for l in f`
/ Go `bufio.Scanner` / Rust `BufRead::lines`). Shipped as a **BODIED Chezzi generator method on the
`native struct Reader`** in `std/io.chz` (`fn lines(self): while true: match self.read_line(): Some(l):
yield l; None: break`). This unblocked the packaging question by enabling **bodied methods on native
structs**: a `native struct` may now MIX Rust-backed bodyless `native fn` sigs (native dispatch) with
pure-Chezzi `fn` methods (compiled to bytecode, routed via `Program::native_methods`, keyed by the
reserved handle's bare name — the enum-method mechanism). `r.lines()` streams lazily by construction (a
generator over `read_line()`; the file is NOT snapshotted), verified on both engines
(`reader_lines_parity` + early-break laziness). Caveat carried forward: the bodied method's BODY is
compiled-but-not-type-checked (the native module skips `check_module`), so the dual-engine RUN test is
the safety net for any future bodied native-struct method.
Also still open: seek / random-access; a `Reader` structural protocol (paired with the `Writer` one —
that pairing is now the "second implementer", so schedule the protocol spike rather than YAGNI it).

### R3. No package manager — **the wall that keeps Chezzi author-only**
`Manifest` is `{name, version, entrypoint}` (`src/manifest.rs`) and the parser **silently ignores**
unknown sections, so a `[dependencies]` block does nothing. The resolver knows exactly two roots — the
project root and `std_root()` (`src/resolver/mod.rs`) — so **a third-party Chezzi library cannot be
imported at all**, except by copying its `.chz` files into your tree. No registry, no lockfile, no
versions, no vendoring.
Everything else in this file is a bad afternoon for a user. This one is a closed door: **nobody can use
anyone else's code, and nobody can use yours.**
`docs/ffi-and-packaging.md §6.1` calls the pure-Chezzi source registry "cheap, do first" — and it is
(a third resolver search path + a fetch cache + a lockfile; **no** ABI/NaN-boxing/`repr(C)` work, which
is only needed for *native* packages). It has never been scheduled. That mis-sequencing — the cheap 90%
stalled behind a native-ABI narrative it does not depend on — is the most consequential finding of the
audit.

### R4. No runtime type tags → no `cast[T]`, no `errors.As` — **WON'T-DO (2026-07-15)**
`Any` (an empty protocol) lets values *in* and nothing *out*; there is no `type()`, no `isinstance`, no
downcast. Protocol **existentials do** give real dynamic dispatch (`examples/poly_method.chz`), so the
sharp edge is narrower than `future.md §14` implies — it is mostly **error discrimination** (see L3)
and dynamic data-walking. **Size: large** (needs runtime type tags on heap objects).

**DECISION: won't-do.** `cast[T]` was pushed back (a general runtime downcast is neither Python nor
Go idiom); the only other use, `errors.As`, is avoidable — model errors as a typed enum and `match` to
discriminate (static, no runtime tags). Large effort, no payoff for a Python-feel scripting language.
Reopen only if dynamic data-walking becomes a real, recurring need.

## Language / concurrency

### 1. Spawn-callee sendability gate — **RESOLVED at check for spawn callee/arg sites** (Task 2a, 2026-07-10)

Spawned tasks **are** usable today: a nested `fn` or closure works as the direct callee of `spawn f()`
(the task runs it; its captured cells are **deep-copied** to isolate them — see
[`concurrency.md §7`](concurrency.md)), and it may capture anything **sendable**: scalars, `str`,
`List`/`Map`/`Set`/`tuple`/structs of sendables, `Channel`/`Shared`/`RwShared`/`Atomic` handles, a
`std.cancel` `Token`, a `.iter()` cursor, and (read-only) module globals. Verified: a task capturing a
`List` or a `Shared` runs fine.

**Was the gap:** the checker's spawn-sendability gate covered `spawn:` / `parallel:` **block** bodies but
**NOT the free captures of a `spawn f()` callee** (closure or nested fn). A callee capturing a `ref T` /
`Ref[T]` and mutating it checked OK yet **ran and silently isolated the write** (a stale-value soundness
bug), contradicting `concurrency.md §7`.

**Fixed (Task 2a):** the checker now records each closure/nested-fn value's non-sendable **local**
captures at its decl site (keyed by binding, using the same `free_names_*` over-approximation the runtime
uses to build captures) and, at a `spawn <name>()` **callee** or `spawn f(<name>)` **arg** site, emits
the verbatim block-form error per captured non-sendable local. A captured **`ref`** is now a clean
compile error at both the callee and the arg site, consistent with the block form. A **module-global**
`ref` is a read-only global (scope-0 exclusion), **not** a capture — never gated. Paired with the
permissive `sendable(Func)` flip (#2), closures-as-data type-check while a captured `ref` is rejected.
An **indirectly**-crossing ref-capture (inside a struct field / `Channel[fn]` value) slips this
check-site gate but is caught by the Task-2b runtime backstop (#2) — no silent `ref` path remains.

### 2. Closures as data — **RESOLVED: RUNTIME (B3.3) + checker gate (Task 2a) + indirect ref-capture runtime backstop (Task 2b, 2026-07-11) all landed**

**Runtime (DONE):** the airlock lowers a closure/bare-`fn` **by value** everywhere — its `proto`
(immutable → shared) + its captures deep-copied recursively into fresh per-task cells + its home-module
index, never a by-reference heap handle — on **both** engines identically (`WireValue::Closure`/
`WireValue::Func`, kept distinct so `str` still renders `<fn NAME>` vs `<closure>`). So a `spawn f()`
callee whose captured environment contains a **nested** closure/`fn` (or is itself a bare `fn`) now runs
cleanly instead of faulting at the airlock.

**Checker (DONE — Task 2a):** `sendable(Ty::Func)` is now **permissive** (a closure crosses by value),
so a **`Channel`/`Shared` element type** of `fn(...)->...` type-checks (`Channel[fn(int)->int]` is
accepted; `channel_of_closures` and a factory closure sent over a channel both run). The per-closure
capture check moved to the airlock **sites** (#1). `ref T`/`Ref[T]` stays non-sendable regardless (use
`Shared[T]`/`Atomic`/`Channel` for cross-task shared mutation).

**Runtime backstop (DONE — Task 2b):** the bare `fn` type cannot carry its captures, so a closure
whose captures include a `ref`/`Ref` that reaches the airlock **indirectly** — inside a struct field
(`Channel[Holder]` where `Holder` has a `fn` field), or through a `Channel[fn]` value — type-checks and
used to **silently deep-copy** the ref (the write vanished). The airlock's two closure-serialization
arms (`to_wire_depth` for `Channel.send`/spawn args, `to_snap_depth` for the M:N snapshot) now scan a
crossing closure's **entire capture graph** (top-level or nested inside a captured
`List`/`Tuple`/`Map`/`Set`/struct/enum/newtype/`Cell`/nested closure), and a `Ref` anywhere in it
raises the **recoverable** runtime error `cannot send a non-sendable ref/Ref captured by a closure
across tasks — use Shared/Atomic/Channel` — **byte-identical on both engines**. Scoped to the closure
arms ONLY: a **module-global** `ref` crosses via the module-globals snapshot (not a closure capture), so
it is never scanned and continues to deep-copy. Together with the Task-2a checker gate, **no silent
`ref` path remains**.

### 3. `Executor.submit` coop-vs-M:N capture-sharing divergence — **RESOLVED (2026-07-11)**

**Was the gap (B3.3 follow-up):** on the cooperative engine `Executor.submit` queued the submitted
closure's own heap `Handle` (captures **shared by reference**, same heap, bypassing `to_wire`), while
`--parallel` wired it **by value** (`WireValue::Closure`). This broke the sacred serial==M:N invariant:
a submitted closure capturing a non-sendable `ref`/`Ref` (directly or via a nested closure) or a live
generator ran silently on serial but faulted on M:N, and a submitted closure mutating a captured
collection observed the mutation on serial but was isolated on M:N (a silent value divergence). The
by-handle branch had been kept deliberately to mirror the tree-walk `interp` oracle.

**Fixed:** `src/vm/netio.rs` now routes **both** engines through `wire_callable` → `to_wire`, exactly
like plain `spawn`. The submitted closure crosses **by value** on the cooperative engine too — captures
deep-copied + isolated at submit time, and the ref/Ref + generator airlock enforcement runs — so serial
and M:N behave identically for every submitted closure. The `interp` oracle was removed, so the by-handle
preservation was pure divergence and is retired. The submit-time generator reach-gate and the drain-time
re-gate (`gate_executor_queue`) are unchanged (reachability is proto-based over the shared `Arc<Program>`,
so switching the queued kind `Handle`→`Closure` leaves verdicts unchanged). Tests:
`executor_submit_{ref,generator}_capturing_closure_faults_both_engines`,
`executor_submit_mutating_closure_isolated_parity`, `executor_submit_sendable_closure_runs_parity`
(`src/vm/parity_tests.rs`), and the rewritten `executor_cooperative_submit_isolates_captures_by_value`.

## Stdlib

Coverage today is *broad* (math, fs, os, time,
datetime, process, rand, regex, request, net, ffi, encoding, crypto, uuid, json, collections, iter,
cmp, string, path, ref, cancel, concurrency); the gaps below are **depth / ergonomics**, not missing
domains. Canonical surface: [`docs/stdlib.md`](stdlib.md).

Discipline reminder (from `CLAUDE.md`): new builtin types/ctors/fns go in their owning `std.*` module
(import-gated), NOT the global reserved namespace. Each item here is its own milestone with a
failing-then-green test + two-engine (serial + M:N) runtime verify.

## Ranked by hit-rate (most-used script surface first)

### 1. String formatting
- ~~Number format-spec in interpolation~~ — **SHIPPED** (`src/fmtspec.rs`, `Op::ToStrFmt`,
  `docs/syntax.md §10`): the full Python mini-language, `{x:.2f}` / fill / align / width / `d f x X b o
  e %`. This entry sat here as "the single biggest ergonomic gap" long after it landed — the audit's
  cautionary tale. It also largely **obsoletes** the next bullet (`"{s:^10}"` is `center`).
- `str.pad_right` / `center` / `ljust` / `rjust` / `zfill` — now only *method spellings* of what format
  specs already do. Downgraded: alias sugar, not a gap.
- ~~`str.capitalize` / `title` / `swapcase`. No `rsplit`, no `split` with a limit, no split-on-whitespace-run.~~
  **SHIPPED** as `std.string` free fns (`std/string.chz`): `capitalize` / `title` / `swapcase` /
  `rsplit` / `split(s, sep, maxsplit=-1)` / `split_whitespace` — Python semantics, free-fn-only.
- ~~`str.find(sub, from_index)` (only `index_of` from 0).~~ **SHIPPED** as `std.string.find(s, sub,
  from_index)`; `index_of` is now `find(s, sub, 0)`.

### 2. List / iter ergonomics — many small additive holes
- ~~`List.min` / `max` / `min_by` / `max_by`~~ **SHIPPED** (methods on `List[T]`, `where T: Comparable`;
  `min_by`/`max_by` take a `fn(T) -> K` key; empty faults `min()/max() of empty list`). `iter.min` / `max`
  still open (only `cmp.min/max` of two) — separate wave.
- ~~`List.first` / `last`; non-mutating `reversed()` (only in-place `reverse`); `insert(i,x)` /
  `remove_at(i)`~~ **SHIPPED** (`first`/`last` → `Option[T]`; `reversed()` returns a NEW list;
  `insert` Python-clamps; `remove_at` returns the element, faults OOB).
- ~~`unique` / `dedup`, `chunk(n)` / `windows(n)`, `take_while` / `drop_while`, `count(pred)`,
  `position(pred)`~~ **SHIPPED** (`unique`/`dedup` return a NEW list — first-occurrence dedup vs
  consecutive-run collapse; `chunk`/`windows` return `List[List[T]]`, `n<=0` faults, `windows` `n>len`
  empty; `take_while`/`drop_while`/`count`/`position` are predicate methods that snapshot the receiver).
  Still open: `group_by`, `partition`, `flat_map` (need method-own type args / Map / tuple returns — a
  separate higher-risk wave).
- Map: `get_or(k, default)` / `setdefault`, `items()`, `map_values`, `filter`. Set: `is_subset` /
  `is_superset` / `is_disjoint`.

### 3. Lazy iterators (itertools) — **SHIPPED (2026-07-16)**
- ~~No lazy adapters: `count` / `cycle` / `repeat` / `chain` / `islice` / lazy `map`/`filter`/`take` as
  `Iterator[T]`. `std.iter` is all-eager `List`.~~ **SHIPPED** as pure-Chezzi generators in `std.iter`:
  `count(start=0, step=1)`, `repeat(x, n=-1)`, `cycle(xs)`, `chain(a, b)`, `islice(it, stop)`, and the
  lazy `imap`/`ifilter` (named to avoid the eager `map`/`filter` — Chezzi has no overloading). Infinite
  sources (`count`/`repeat`/`cycle`) terminate under `islice`. Follow-ups: `chain` is two-arg only
  (varargs / list-of-iters later); `take(it, n)` alias dropped (collides with eager `take`; `islice`
  covers it).
- **OPEN — lazy `itakewhile`/`idropwhile` over `Iterator[T]`.** The eager `take_while`/`drop_while`
  shipped as `List[T]` methods (§2, 2026-07-16), but `std.iter` has no lazy while-adapters (it has lazy
  `imap`/`ifilter`/`islice` but no while-form). Python `itertools.takewhile`/`dropwhile` are lazy;
  add the `i`-prefixed generators here in a later §3 wave (same pure-Chezzi generator shape as `imap`).

### 4. IO / files
- **Interactive CLI — SHIPPED** (see *Interactive CLI* below): `chezzi run` streams stdout, `io.flush()`
  and `io.input(prompt)` exist, and a prompt appears before its blocking read.
- **Buffered output — SHIPPED (R2).** `buffered(stdout())` (Go's `bufio.NewWriter` escape hatch) batches
  writes; the module-level `io.flush()` stays an honest no-op for the unbuffered stdout sink, and the
  *Writer*'s `flush()` is the real drain.
- **Writer / file handles — SHIPPED (R2).** `create`/`append` openers + a write-only `Writer`
  (`write`/`write_bytes`/`flush`/`close`) — append-to-an-open-file + streaming write now exist. Whole-file
  read stays (`std.io`: `read_file`/`read_bytes` ≤64 MB, `write_file`/`write_bytes` uncapped).
- **Reader / file handles — SHIPPED (R2b).** `open(path)` opener + a read-only `Reader`
  (`read_line`/`read_bytes`/`close`/`lines`) — line/chunk streaming of a large file (past the 64 MB
  whole-file cap) now exists, the read twin of R2's `Writer`. `lines() -> Iterator[str]` (a lazy
  generator over `read_line()`) is SHIPPED as a bodied Chezzi method on the native handle
  (`for ln in r.lines():`).
- **Read-all-stdin; char read — SHIPPED.** `io.read_all() -> str` drains all remaining stdin to EOF
  as one `str` (Python `sys.stdin.read()`; `""` at clean EOF; non-UTF-8 = fault, no stdin `read_bytes`
  hatch), and `io.read_char() -> Option[str]` reads one Unicode scalar as a 1-char `str` (`None` at
  clean EOF; partial/invalid UTF-8 = fault). Both are siblings of `read_line` — same shared stdin
  source, same task behavior (not offloaded; inherit the v1 pin-a-worker limit).
- **fs grab-bag — SHIPPED.** `fs.canonicalize(path) -> Result[str]` (resolves symlinks + `.`/`..`
  against the real filesystem — requires the path to EXIST, distinct from the lexical `path.normalize`),
  `fs.chmod(path, mode: int) -> Result[nil]` (unix permission bits, unix-only), and
  `fs.atomic_write(path, contents) -> Result[nil]` (same-dir temp + `rename`; observer-atomic +
  mode-preserving, not fsync-durable) all now exist.

### 5. Numbers / math
- **SHIPPED (Wave-3 Run E):** `gcd`, `lcm`, `sign`, `trunc`, `hypot`, `cbrt`, `factorial`, `comb`,
  `perm`, `parse_int_base(s, base)` (int-from-base, base 0 or 2..=36 w/ `0x`/`0o`/`0b` prefixes), plus
  `math.inf` / `math.nan` constants. Python `math` semantics; `factorial`/`comb`/`perm`/`parse_int_base`
  return `Result[int]` (clean `Err`, never a fault, on bad domain or i64 overflow). See `stdlib.md §std.math`.
- **`divmod` SHIPPED** — Python `(q, r)`. Landed NOT by expanding the native seam (`NativeRet` still has
  no `Tuple`) but as a **bodied Chezzi fn** in `std/math.chz` (`fn divmod(a, b) -> (int, int): return
  (a / b, a % b)`) — the first user of the hybrid native+Chezzi module form (bodyless `native fn`s and a
  bodied `fn` in one std file; see `syntax.md`). Uses Chezzi's own C-style `/` (truncating) and `%`
  (dividend's sign), so it is `(a / b, a % b)` — NOT Python's floor `divmod` (`divmod(-7,2)` is `(-3,-1)`
  here, `(-4,1)` in Python); a Python-floor variant would drift from Chezzi's own operators, a worse
  surprise than matching them.
- No **decimal / bigint**. `int` is a checked i64 (overflow FAULTS, never promotes), so a big-number or
  exact-money program simply cannot be written — there is no workaround. (Python: `int` is arbitrary
  precision + `decimal`; Go: `math/big`.) Rare in scripting; deferred, but it is a hard wall, not a
  slow path.

### 6. OS / system
- ~~os: `setenv`, `chdir`, `getpid`, `platform`, `hostname`, `environ()`, `home_dir`, `temp_dir`~~ —
  **SHIPPED** (2026-07-16): all eight in `std.os`. Queries (`getpid`/`platform`/`hostname`/`home_dir`/
  `temp_dir`/`environ`) are engine-agnostic; `setenv`/`chdir` mutate global state (see below). Still
  open: `os_name` alias for `platform` (trivial follow-up), Windows `USERPROFILE` fallback for
  `home_dir` (unix-focused today), metadata-reader.
- **No cleanup story at all** (three bullets that are really one): no temp-file/temp-dir creation, **no
  signal handling / `atexit` hook**, and `os.exit` does **not** run `defer`s. So a program that must
  clean up on Ctrl-C or on exit has no reliable path. (Python: `tempfile` + `atexit` + context managers;
  Go: `os.CreateTemp` + `defer` + `signal.Notify`.)
- ~~**No TTY detection**~~ — **SHIPPED** (2026-07-16): `io.isatty()` / `io.isatty_stdin()` /
  `io.isatty_stderr()` `-> bool` (via `std::io::IsTerminal` on stdout/stdin/stderr) let a CLI colorize
  only when not piped. Terminal size / echo-off (password prompts) remain a deliberate second step.
- **`os.env` and `process.cmd` disagree** (PARTIALLY RESOLVED 2026-07-16): `os.env`, `os.environ`, and
  `os.setenv` are now mutually consistent — all three read/write the SAME injected `HostConfig` env map,
  so a `setenv("X","V")` is observed by both `env("X")` and `environ()["X"]`. The map is **shared**
  (`Arc<Mutex<…>>`) across M:N workers, so a `setenv` from inside a task is visible to the parent +
  siblings — process-global, matching the serial engine and Python/Go (serial == M:N, no parity break);
  `environ` sorts by key so both engines emit identical output. What remains is the process-boundary
  axis: `process.cmd` shells out with the REAL inherited process env, so a `setenv` (HostConfig-only) is
  NOT seen by a child, and under a synthetic host config `os.env("X")` can differ from
  `process.cmd("echo $X")`. Bridging that would require writing the real process env at `setenv` (racy,
  edition-2024-unsafe `std::env::set_var`) — deliberately not done.
- fs: ~~recursive `walk`~~ — **`fs.walk(path) -> Result[List[str]]` SHIPPED** (deterministic per-dir
  sorted flat list, does NOT follow symlinked dirs; `native/fs.rs`). `remove_dir_all` (intentionally
  omitted today — see `stdlib.md §std.fs`). ~~metadata READ (mtime / permissions / size-struct)~~ —
  **`fs.stat(path) -> Result[FileInfo]` SHIPPED**: a native `FileInfo` struct (size/mtime/mode/is_dir/
  is_file/is_symlink), follows symlinks like `stat`/`os.stat`. `fs.chmod` still SETS permission bits
  (`fs.stat().mode` now READS them).

### 7. Crypto / encoding
- crypto: `sha256` / `sha256_bytes` / `sha1` / `sha1_bytes` / `sha512` / `sha512_bytes` / `md5`, plus
  `hmac_sha256(key, msg)` (RFC 2104, over the SHA-256 primitive). ~~secure-random-bytes / token~~ —
  **SHIPPED**: `secure_bytes(n) -> bytes` / `token_hex(n) -> str` (Python `secrets`), OS `getrandom`,
  **fail-closed** (recoverable fault, never weak fallback), 1 MiB cap; `token_urlsafe` (base64url)
  deferred. Missing: password hashing (bcrypt/argon2); `hmac_sha1`/`hmac_sha512` not shipped (add if a
  caller needs them — they want a block-size param + `&[u8]` adapters). All hand-rolled zero-dep today,
  so each is real work.
- encoding: no gzip / zlib (new dependency). ~~no CSV~~ — **CSV SHIPPED** as a NEW pure-Chezzi module
  `std.csv` (`parse(text) -> List[List[str]]` / `format(rows) -> str`, RFC 4180 quote state machine,
  round-trip proven; `std/csv.chz`, NOT `std.encoding` which is file-backed native). Deferred v1
  follow-ups: streaming/Reader, header→Map mapping, custom-delimiter/TSV `parse_sep`. Arbitrary-**bytes** base64 round-trip →
  **DONE (R1)** (`base64_encode_bytes`/`base64_decode_bytes`); hashing a *file* → **DONE (R1)**
  (`io.read_bytes` + `crypto.sha256_bytes`). Not added: hex / URL-safe bytes twins (~6 lines each, on demand).
- **URL parsing read-half — SHIPPED**: `query_decode(q) -> Map[str,str]` (dup key last-wins, `+`/`%20`
  → space, malformed escape kept raw) and `url_parse(u) -> Map[str,str]` (lexical
  scheme/host/port/path/query/fragment, components stay encoded, port a string) now round out
  `url_encode` / `url_decode` / `query_encode`. (Correction: the "Small, pure-Chezzi" label here was
  wrong — `std.encoding` is a FILE-BACKED NATIVE module; all members are bodyless `native fn` decls in
  `std/encoding.chz` implemented in `src/native/encoding.rs`. A pure-Chezzi fn there is dead code.)

### 8. Net — *and `std.net` is `--parallel`-only, which is a standing serial≠M:N divergence*
- TCP (`std.net`) + HTTP-client (`std.request`) only. No UDP, no HTTP **server**, no DNS-resolve
  exposed, no raw TLS socket (`request` does HTTPS internally via ureq). Also missing: unix-domain
  sockets, `shutdown()` half-close, socket options (`set_nodelay`, `SO_REUSEADDR`, keepalive),
  `Socket.peer_addr()`.
- **The HTTP-server blocker was not "no framework"** — you *can* hand-roll one on `listen`/`accept`/
  `read`/`write`. The blocker was that the socket seam was **`str`-only**, so a hand-rolled server could
  serve JSON and could not accept an image. **FIXED by R1** (`Socket.read_bytes`/`write_bytes`, 2026-07-14):
  binary sockets work byte-exactly. HTTP *fetch* of a binary body — a separate, `std.request`-side gap —
  is now **also DONE** via `request.get_bytes` (2026-07-15, byte-exact `into_reader().read_to_end`).
- **`std.net` requires the M:N engine**: off it, a would-block op returns `Err("read would block:
  std.net sockets require the --parallel engine")` (`src/vm/netio.rs`). So the same TCP program behaves
  differently on `--serial` vs the default engine. This is an *accepted design fallback*, not a bug —
  but it must be written down, because §"Audited residuals" previously claimed the task-stdin bug was
  "the only known serial≠M:N divergence", and that was **wrong as written**.

### 9. Date/time — `parse_iso8601` LANDED; `strptime`/`from_string` remain
**`datetime.parse_iso8601(s: str) -> Result[DateTime]` shipped** (pure-Chezzi, the exact inverse of
`to_iso8601`): parses ISO-8601 / RFC-3339 — date-only, `'T'`/`' '` separator, optional `Z` or
`±HH:MM` offset (normalized to UTC), optional truncated `.fff` — with clean `Err` on malformed /
out-of-range fields. So a script **can** now turn a JSON / HTTP-header / CSV / log timestamp into a
`DateTime`. **Remaining follow-up:** a `strftime`-pattern formatter and a general
`strptime`/`from_string` (format-token vocabulary) — deferred (no token surface in v1, would balloon
scope). Known ceilings: sub-second precision dropped (`DateTime.second` is int), non-`Z` offsets
normalize to UTC rather than round-tripping. (Python: `fromisoformat` done, `strptime` pending; Go:
`time.Parse` layout pending.)

- ~~**No Go-like first-class `Duration` type.**~~ **SHIPPED** as pure-Chezzi `std.duration`
  (`std/duration.chz` + one `include_str!` in `std_embed.rs`; `Duration` is a plain user struct over an
  int of **milliseconds** — no native seam). Constructors `millis/seconds/minutes/hours(n)`, accessors
  `as_millis()/as_seconds()/as_minutes()/as_hours()`, arithmetic `add`/`sub`/`scale`, a Go
  `time.Duration.String()` formatter `to_string()` (`"1h30m0s"`, `"1.5s"`, `"250ms"`, `"0s"`, `"-1.5s"`)
  and its inverse `parse("1h30m")` (Go's looser forms + clean `Err` on malformed), plus `since(start:
  float) -> Duration` and `sleep(d)` convenience over native `std.time`. `sleep_ms`/`timer` stay int-ms
  (additive). Sub-ms ceiling documented (µs/ns → `Err`). Correctness = `parse`/`to_string` round-trip
  vectors in `examples/duration_test.chz` + `examples/duration.chz` golden (both engines). See
  `docs/stdlib.md §5`.

### 10. Missing modules a real script reaches for
- ~~**`std.flag` — CLI arg parsing.**~~ **SHIPPED.** Pure-Chezzi `std/flag.chz`: a Go-`flag`-style
  `FlagSet` (`flag.new()` → `str_flag`/`bool_flag`/`int_flag` → `parse(args) -> Result[List[str]]` →
  `get_str`/`get_bool`/`get_int`/`positionals()`/`usage()`) over `os.args()`. `--name value` /
  `--name=value` / bool-presence / `--` terminator; unknown/missing/non-int → clean `Err`. See
  `docs/stdlib.md §5 std.flag`.
- ~~**`std.log` — levels + timestamps + stderr default.**~~ **SHIPPED.** Pure-Chezzi `std/log.chz`:
  `log.new(min_level=INFO, to_stderr=true) -> Logger` with `debug/info/warn/error(msg)` gated by
  `set_level`, formatting `"LEVEL message"` (Go `slog` levels `DEBUG<INFO<WARN<ERROR`) to stderr by
  default. Timestamps are opt-in/injectable via `set_prefix` (the pure `format_line` core stays
  deterministic — no baked-in clock). See `docs/stdlib.md § std.log`.
- **`std.db` (sqlite).** Absent. Reachable *in theory* via FFI to `libsqlite3` (the opaque `ptr` type
  names `sqlite3*` as its motivating case) but that is a research project, not a workaround. Blocks
  persistence-shaped scripts. **Large.**
- Config formats (TOML/YAML/INI): absent, JSON only. Low priority — JSON + env vars cover it. If ever:
  TOML, not YAML.
- ~~`bisect` / `binary_search` on a sorted `List` (sort/sort_by already exist). ~10 lines.~~
  **SHIPPED.** Pure-Chezzi `std/bisect.chz`: `bisect_left`/`bisect_right`/`bisect` (alias) +
  `insort_left`/`insort_right` over `List[T: Comparable]` (Python `bisect` semantics; left = before
  equals, right = after). No key-fn variant / no bare `insort` alias in v1 (YAGNI). See
  `docs/stdlib.md § std.bisect`.
- ~~`functools.cache` / `memoize` — now *possible* (closures-as-data landed); ~15 lines.~~
  **SHIPPED.** Pure-Chezzi `std/memoize.chz`: `memoize1(f: fn(K) -> V) -> fn(K) -> V` caches per
  distinct arg in a captured `Map` (native ref type, so the cache persists across calls). Single-arg
  only — N-arg would key `Map[tuple, V]` but tuples aren't Hashable map keys yet. See
  `docs/stdlib.md § std.memoize`.
- Runtime templating (`render(tpl, vars)`) — interpolation is compile-time only. Mostly obviated by
  format specs; the residual need is HTML generation, and **if an HTTP server ever ships, the lack of an
  auto-escaping template is an XSS hole**, not an ergonomics gap.

### 11. `std.process` cannot talk to a running child — *the ranked list had no `process` entry at all*
All three members (`cmd`/`run`/`run_args`) call `.output()`: spawn, wait, collect. There is **no
`Popen`/`exec.Cmd` equivalent**, so you cannot pipe stdin to a child, read its output incrementally
(progress from `ffmpeg`, a `tail -f`), set its env or cwd, get its pid, kill it, or run it in the
background. A child producing 4 GB of stdout is buffered entirely in RAM. `stdlib.md §std.process`
admits "Not yet: stdin piping, output streaming, per-process env/cwd overrides" — but that never made
it here. Compounded by the missing `os.setenv`/`os.chdir`: with neither, there is **no way at all** to
control a child's environment or working directory. Needs a `Child` handle (sibling of R2's `Writer`).

## Language features (category added 2026-07-14 — this file previously had none)

Verified against the parser/checker, not the docs. **Not gaps** (checked, and worth recording so nobody
"fixes" them): protocol **existentials give real dynamic dispatch** (trait objects work —
`examples/poly_method.chz`); `defer` is block-scoped and strictly more general than `with` for a
language with no destructors (`future.md §1` rejected `with` and is still right); generators/`yield`,
comprehensions, varargs, default args, keyword args, newtype, type aliases, static methods, enums with
methods — all shipped. The mutability model (aggregates share by reference like Python objects,
`Shared`/`Atomic`/`Channel` for cross-task shared mutation) is coherent. (`ref T`/`Ref[T]` were removed
2026-07-19 — see `future.md §12`.)

### L1. `Result` / `Option` have **ZERO methods** — **DEPRIORITIZED (2026-07-15): not imitating Rust's method surface**
`native enum Option[T]` / `Result[T, E]` (`std/prelude.chz`) declare no methods, and there is no
`Ty::Result`/`Ty::Option` arm in the method-call checker. So there is no `unwrap_or`, `unwrap_or_else`,
`is_ok`, `is_some`, `ok()`, `map`, `map_err`, `and_then`, `expect`. Verified: `Some(1).unwrap_or(0)` →
*"type Option[int] has no method 'unwrap_or'"*. Every `Result`/`Option` is handled with `match` or `?`.
**Small** if ever wanted (the `native enum … native fn` method-table machinery already exists — it is how
`List` works, ~8 native methods) — but **deprioritized**: `match`/`?` is the intended surface, and L3 (the
one thing L1 methods would have "unblocked") is itself won't-do, so there is no downstream forcing it.

### L2. No struct patterns in `match` — **struct match-patterns FIXED (2026-07-15); let/fn-param destructuring still deferred**
**Struct patterns in `match` now work**: `match p: Point(x, y):` binds the fields positionally, mirroring
enum-variant patterns (a struct has exactly ONE constructor, so a lone all-binding `Point(x, y)` arm is
irrefutable ⇒ exhaustive with no `_`). Nested (`Line(Point(x, y), _)`), generic (`Box(v)` on `Box[int]`
binds `v: int`), literal fields (`Point(0, y)` — refutable, needs a `_`/catch-all), and a whole-value
catch-all binding (`rest:`) all work. **Both a BARE `Point(x, y)`** (a local / `from`-imported struct)
**and a QUALIFIED `geo.Point(x, y)`** (the only spelling for a WHOLE-module-imported struct, since the bare
name isn't in scope — symmetric with qualified construction `geo.Point(3, 4)`) destructure. Arity mismatch,
a wrong constructor, an enum-name-collision qualifier (`E.Point`), a non-module qualifier, a 3-part path,
and a DUPLICATE constructor arm are all clean checker errors, never runtime panics. Reserved/native struct
handles (Socket/Ref/Match/…) are **not** destructurable (checker-gated to `StructOrigin::User`, so the
compiler never sees a struct pattern it can't lower). Example: `examples/match_struct.chz`. Landed as a
checker + pattern-compile + VM-lowering change reusing the enum-variant `Pattern::Variant` node (no new AST
node/opcode): `MatchKind::Struct` + `struct_fields_of` (checker/sig.rs), the Struct arms in
`bind_match_arm`/`bind_subpattern`/`check_exhaustive` + the shared `resolve_struct_ctor` (checker/pattern.rs),
and `struct_key_of_pattern` (bare + module-qualified) + the refined `EnsureEnum` guard + the `emit_pattern`
struct branch (compiler/mod.rs).
**Still deferred:** `let`-destructuring is tuple-only (`let Point(x, y) = p` — `StmtKind::Let` carries
`names: Vec<String>`, not a `Pattern`, so it needs a separate parser+AST+let-lowering seam, not this one);
no destructuring in fn params. (Python 3.10 class patterns; Rust/Go destructuring.)

### L3. Error handling: no conversion, no wrapping, no discrimination — **WON'T-DO (2026-07-15)**
**FIRST, the correction that scoped this down (2026-07-15):** a concrete error type WIDENS to the
built-in `Error` existential exactly like Go's `error` interface — a `struct`/`enum` with a
`message(self) -> str` **method** (declared *inside* the block, not a free `fn`) flows into an `Error`
param, into the `Result`-E position (`return Err(MyErr(..))` in a `-> int!` fn), and through `?`
(`inner()? ` where `inner` is `-> int!MyErr` inside a `-> int!` fn). Verified by `check` + `run` on both
engines. So the idiomatic `T!` (= `Result[T, Error]`) style already composes — `?` is NOT broadly broken.

Given that, the three "holes" are narrow and NOT worth building:
- **`?`-time conversion.** Only concrete-E1 → *different* concrete-E2 is closed (`T!IoErr` called from
  `T!DbErr`). Concrete → `Error` widens fine (above). Cross-concrete auto-conversion is rare and
  arguably SHOULD be explicit (that's a real decision, not boilerplate) — a Rust `From`-style machinery
  is exactly the imitation we don't want. **Won't-do.**
- **Wrapping / cause chain** (`source()`/`Unwrap()`). Nice-to-have, not blocking. **Defer.**
- **Downcast out of the `Error` existential** (`errors.As`). Needs **R4**, which is won't-do — avoid by
  keeping the error a typed enum and `match`ing it before laundering to `Error`. **Won't-do.**

### L4. ~~No `const`~~ — **`const` SHIPPED (2026-07-17)**; visibility still open
- ~~No `const`/`final` keyword.~~ **SHIPPED.** `const T` is an immutable *binding* modifier in the
  same type-slot as `ref` (`PI: const float = 3.14`) — the checker rejects any later reassignment
  (`=` + every compound). Immutable binding, NOT a compile-time constant (runtime RHS is fine, JS
  `const`/Java `final` semantics), and **shallow** (freezes the name; a `const` container's contents
  stay mutable). Locals + module globals only; rejected on params/`:=`/destructuring and `ref const`.
  Const-ness rides `ModuleSig.const_values` so a from-import/qualified rebind of a `const` global (or a
  native constant `math.pi`/`e`/`inf`/`nan`) reports it as const. Mirrors the `ref` sidecar end-to-end
  (`const_decls`); compile-time-only, zero VM/parity change. See `syntax.md §const T`,
  `examples/const_binding.chz`.
- **STILL OPEN — visibility.** No `pub`/private (every name in a module is importable). (Go:
  capitalization export; Python: convention + `__all__`.) Small-to-medium (resolver + `ModuleSig`
  filter). **Deferred**: it guards a boundary only **R3** (package manager) opens — with one author and
  no external importers, enforced privacy protects nothing yet. Do it when R3 lands. (`_`-prefix is the
  chosen spelling — Python-consistent, no new keyword, and std already uses it by convention.)
- Struct-**field** immutability (a `const` field) is a separate, unshipped axis (fields are all mutable).

### L5. Operator-protocol holes
The reserved set (`Add Sub Mul Div Mod Neg Arithmetic Comparable Stringable Hashable Index IndexSet
Slice Contains Iterator Iterable Convert Any Error`) covers arithmetic, ordering, indexing, slicing,
membership, iteration, hashing, display. Missing: **`Eq`** (`==`/`!=` cannot be overloaded — and see
**B2**, the checker is *permissive* about them), bitwise/shift protocols, and a call operator. Small
each. **`Contains`** (`x in my_struct` via `contains(self, item) -> bool`, Python's `__contains__`) —
**FIXED**: a user struct/enum with a `contains(self, item) -> bool` method makes `x in that_value`
dispatch to it, yielding `bool`; container `in` (list/set/map/str) is unchanged.

### L6. Smaller, confirmed
- Enums carry **no discriminant/value**, no variant iteration, no int conversion (Go's `iota`, Python's
  `Enum.value`). Small.
- No labeled `break`/`continue` (Go has them; Python doesn't). Small.
- No generator *expressions* (`(x for x in xs)`) — comprehensions are `[]`/`{}` only; `yield` covers it
  verbosely.
- No walrus in expression position (`if (n := f()) > 0`) — `:=` is a statement.
- No **struct embedding / extension methods**: methods may only be declared in the type's own body (no
  `impl` block), so you cannot add a method to a builtin or to another module's type, and "composition
  not inheritance" means hand-forwarding every delegated method. (Go's embedding is *the* composition
  mechanism.) Medium.
- Protocols have **no default method bodies** (a protocol method with a body is a parse error) → no
  mixins. Go's interfaces don't either; Python ABCs do. Small, if ever wanted.
- **Not a gap:** spread/unpack (`f(*args)`) was deliberately dropped in `spec.md` and varargs +
  `.concat`/`.merge` cover it.

### L7. Sendability-bounded protocol existentials — the sound way to admit `Channel[Error]` (✅ LANDED 2026-07-20)

**⚠️ SUPERSEDED by Task 2 (2026-07-21, backlog item 1 above).** The `Error`-only / "Rust `Send`, not
Go" framing below was **reversed**: all user protocol existentials are now sendable (Go `chan interface`
parity), `sendable_bounded` is deleted, and the genuine-non-sendable gate is the **runtime airlock**
(FFI/native handles), not a checker widening-site sweep. The "a struct satisfying `Error` yet holding a
non-`Error` protocol field launders past the gate" concern below is moot — that struct is genuinely
sendable now (a protocol field crosses by deep value copy); a field holding an FFI/generator handle is
caught at the runtime airlock. The historical Error-only record is kept below for provenance.

**⚠️ Airlock value-store gap — CLOSED 2026-07-23.** The "caught at the runtime airlock" claim above
held for the spawn-**arg**/capture/`submit`/worker-return paths (they pair `to_wire_at` with
`ensure_crossable`) but was **false** for the cross-heap **value-store** paths: `Channel.send`/
`try_send`/`wait:`-send-arm and every `Shared`/`RwShared`/`Atomic` construct/set/update/store/CAS
called bare `to_wire_at` with NO handle reject. An FFI/native/module handle sent over a channel or
stored in a `Shared` therefore crossed silently on `--serial` (and even executed) while M:N
reconstructed a garbage cross-heap `GcRef` — serial≠M:N + type confusion. Fixed by routing every
value-store site through a single `Vm::to_wire_crossable` helper (`= to_wire_at` then
`ensure_crossable`, `src/vm/sched.rs`), so both engines now reject identically and recoverably at the
send / store / construction site with the `a module handle cannot cross` message. Legit
`Channel`/`Shared`/`Executor`/socket handles map to shared-`Arc` wire arms (`has_handle()` == false)
and still cross unchanged (regressed by `positive_*` parity tests). **UPDATE 2026-07-23:** native
(`Obj::Native`) + FFI (`Obj::Cffi`) fn values were later moved OFF this reject — they are pure code and
now cross the airlock BY VALUE / shared `Arc` at every site (`WireValue::Native`/`Cffi`), so the sole
remaining reject is a genuine `Module` handle (see the session log below).

**✅ LANDED (branch `feat/l7-sendable-error`, commits `c1b4ab4` core gate · `997e642` direct-literal
guard · `2b29ed3` regression/residual tests · `ba2ea7c` recover diagnostic).** Surface shipped:
**`Error`-only, sendable-bounded by default** (not all protocols — that over-rejects in-task
non-sendable protocol values and diverges from Rust's opt-in `dyn Error + Send`; reference model is
Rust's `Send`, not Go's share-by-reference channels). `Channel[int!]` / `Channel[Error]` now
type-check and cross a task boundary on both engines; a non-sendable error witness is **rejected at
the widening site**, never laundered.

Design ("Option B", 5 edits, all `src/checker/`): `sendable_rec`'s `Ty::Protocol` arm returns
`self.sendable_bounded(p)` (`== "Error"`, the single surface knob); the three Error-**inference**
synthesis sites (`fill_ret` sig.rs, `default_expr_result_e` pattern.rs, `join_err_slot` sig.rs) default
to the `Error` existential **only if the concrete payload is sendable, else preserve the concrete
type** (so in-task use of a non-sendable error stays legal — the concrete survives to the boundary);
the explicit/direct-literal widening chokepoint (`assignable`'s `Protocol` arm) **rejects** a
non-sendable concrete when the target is sendable-bounded. Every value write-site routes through
`assignable`, so that one guard covers all explicit widenings including `?`-propagation.

Clarifications learned in implementation: (1) `Iterator[T]` is `Ty::Struct`, **structurally sendable**
— a live generator is handled by the runtime reach-gate, not the checker — so the *type-level*
non-sendable witness is a struct holding a **non-`Error` protocol / `Module` field**, not a generator
field (the old F2 framing below overstated the generator case). (2) A `recover:` block's error slot is
the (now sendable-bounded) `Error`, so `recover: f()?` requires `f`'s error to be sendable too; the
diagnostic distinguishes *doesn't-satisfy-Error* from *satisfies-but-non-sendable*.

**Deferred follow-ups (non-blocking):** (a) the direct-literal send rejection surfaces as a generic
type-mismatch (`expected Result[int], found Result[GErr]`), not the friendly "must be sendable" hint —
`assignable` returns `bool` with no reason channel; wire `sendable_error_hint` at the send call site
later. (b) `join_err_slot` is branch-order-sensitive for 3+ branches mixing sendable/non-sendable
`Error` payloads (over-rejection, not a soundness gap). (c) Full Option-B for `recover` (preserve the
concrete error in the recover *result* so in-task non-sendable recover stays legal) — deferred as rare
+ risky; the current construct-imposed `Error` slot rejecting non-sendable is consistent with explicit
annotations. (d) Per-use `+ send` bounding (Rust's `dyn Draw` vs `dyn Draw + Send`) if a second bounded
protocol ever appears.

*Original deferral note (historical — superseded by the landing above):*



**Motivation (F2, 2026-07-18 bug-hunt).** `Channel[int!]` / `Channel[Error]` are rejected today because a
protocol existential is non-sendable (`sendable_rec`, `src/checker/proto.rs`). A one-line whitelist of the
built-in `Error` existential was tried and **reverted as unsound**: the existential *erases field-level
sendability*, so a struct that satisfies `Error` yet holds a non-sendable field (a **live generator**,
a non-`Error` protocol / `Module` field) launders past the gate that the concrete `Channel[MyErr]` correctly rejects —
check-OK-then-run-fault (verified: `Err(GErr(gen()))` over `Channel[int!]` type-checked then faulted `a
generator cannot be sent across tasks`, both engines). The current **conservative rejection is correct**;
the workaround (a concrete sendable error type) already works: `Channel[int!str]`, `Channel[int!MyEnum]`,
`Channel[Result[int, MyErr]]` all type-check and send `Err(...)` across a spawn today, both engines. So this
is a **generalization, not a live gap** — deferred, not urgent.

**Why Rust, not Go, is the reference.** Go has NO static sendability check — you may send *anything* over a
channel (an `interface{}`, a pointer, a mutex), because Go channels **share by reference** (`chan *T` hands
both goroutines the same pointee) and defer safety to the race detector + discipline. Chezzi is the
opposite: the airlock **deep-copies** on send (tasks are isolated — value semantics, no shared mutable
memory except through `Shared`/`Channel` handles; this is why the B3 module-global mutation was *lost* on
M:N). That is **Rust's `Send`, not Go's channel model**. In Rust a bare `dyn Error` is not `Send`; you write
`Box<dyn Error + Send>` and the compiler then forces every concrete error stored into it to be `Send`. That
`+ Send` bound is exactly the design below.

**The feature: a sendability-bounded protocol existential.** Let a protocol (or a use-site) be marked
sendable-bounded, meaning the existential is itself sendable AND every value widened into it is required to
be sendable. Then `Channel[Error]` is sound: a `Ref`/generator-carrying witness is **rejected at the
widening**, not laundered — `sendable_rec` can safely return `true` for the bounded existential.

**The work (checker-side, well-scoped but real).** Chezzi protocols are **structural** (Go-style), so
widening to an existential is frequently *implicit* (a struct used where `Error` is expected — e.g. the
`Err(GErr(...))` argument in the F2 repro). Every implicit widening site becomes a sendability check point:
- add a sendable-bound marker to protocol types (a bounded existential `Ty`, or a per-use `+ send` flag);
- at every place a concrete type is coerced to a sendable-bounded existential (call args, `Err(...)`/`Ok(...)`
  payloads, struct fields, returns, channel `send`), require `sendable(concrete)` and error otherwise —
  mirroring the `+ Send` propagation;
- flip `sendable_rec`'s `Ty::Protocol` arm to `true` for a sendable-bounded existential (only);
- decide the surface: is `Error` sendable-bounded by default (simplest — errors are almost always plain
  data), or opt-in per protocol / per use? Default-bounded `Error` gives `Channel[int!]` for free but would
  reject a today-legal `Err(struct-with-Ref)` used purely in-task — measure that blast radius first.

**Risk / why POST-FREEZE.** This is checker surface with **real false-positive risk** (every widening site
must be found; a missed one is a soundness hole, an over-eager one rejects legal in-task code). Do NOT
attempt before the JIT freeze. The concrete-error-type workaround covers the practical need until then.
Related: [B2](#b2--between-disjoint-types-type-checks-a-proposed-tightening-not-a-clear-bug) (another
typed-language tightening), and the note that **dropping `ref`/`Ref` was the WRONG lever *for F2*** — it
would not close the generator-field laundering that F2 is about, so this milestone stands on its own.
(NOTE 2026-07-19: `ref`/`Ref`/`std.ref` were later removed *separately*, on minimalism/coherence
grounds — they only added scalar aliasing over Chezzi's Python object model — **not** as an F2/sendability
fix. That removal neither addresses nor blocks this L7 milestone; the two are orthogonal.)

## Tooling / ecosystem (category added 2026-07-14 — this file previously had none)

The CLI ships exactly 8 commands (`init run test check tokens ast docs help`). **R3 (no package
manager) is the headline and lives above** — it is the one gap that keeps the language author-only.

### T1. ~~Installing `chezzi` produces a binary that can't find its own stdlib~~ — **FIXED**
> **FIXED** (`fix(resolver): embed std/ so an installed chezzi finds its own stdlib`). `std/**/*.chz` is
> now `include_str!`'d into the binary (`src/resolver/std_embed.rs`, the same pattern the CLI already
> used for the `docs/*.md` topics), and *every* `std.*` source read — `Builder::visit` (incl. the
> always-linked `std.prelude`/`std.ref`) and `Builder::visit_native_file` (the file-backed natives
> `math`/`regex`/`io`/…) — routes through the new `resolver::std_source(dotted)`: **`$CHEZZI_STD` (dev
> override, exclusive) → the embedded stdlib.** The build-time `CARGO_MANIFEST_DIR/std` path is no longer
> in the *read* chain, so an installed `~/.cargo/bin/chezzi` keeps working with the checkout moved or
> deleted (verified E2E: `mv std std.bak`, then `chezzi run` + `chezzi run --serial` a program importing
> `std.math` / `std.regex` / `std.concurrency.collection` — byte-identical on both engines). A missing std
> module now says *"no such module in the stdlib"* instead of leaking the build machine's path. The
> hand-written table is rot-guarded by `embedded_std_table_matches_disk` (embedded key set **and**
> contents == the on-disk `std/` tree): **add a `std/foo.chz` and that test fails until you add its
> `include_str!` line.**
>
> Residual: a **pre-built** binary plus an edited `std/*.chz` is stale until rebuilt (`cargo run`/`cargo
> test` rebuild automatically via `include_str!`; the documented escape is `CHEZZI_STD=./std`).
>
> Residual 2 (**open**, found by the review panel, deliberately NOT fixed): `LoadedModule::is_std`'s
> ENTRY backstop still keys on `path_under_std_root` → `std_root()` → the build machine's
> `CARGO_MANIFEST_DIR/std`, which on an installed binary does not exist (`canonicalize` errs → `false`).
> So type-checking a stdlib file **as the entry** (`chezzi check ./std/concurrency/collection.chz` from
> an installed binary) loses stdlib auto-privilege and reports bogus "unknown type" errors on its bare
> reserved types (`RwShared`, `Map`). Before the embed this path failed loudly at `std.prelude` instead.
> Real, but the fix is re-keying `is_std` off the dotted path — a resolver change larger than the bug,
> with no plausible user (nobody entry-checks the stdlib from an installed binary). Revisit if one appears.

The original finding: `std_root()` = `$CHEZZI_STD` else **`env!("CARGO_MANIFEST_DIR")/std`**
(`src/resolver/mod.rs`), and the `std/*.chz` files were **not embedded** (only `docs/*.md` were
`include_str!`'d). So `cargo install --path .` yielded a `~/.cargo/bin/chezzi` that read its stdlib from
*the source checkout's build-time path*: move or delete the repo and every `import std.*` broke. The code
comment admitted it deferred "a real install story to M6, when `std/` actually ships content" — M6
shipped; the install story did not.

### T2. ~~`chezzi repl` is a stub that ERRORS — while `--help` advertises it~~ — **FIXED (de-advertised)**
> **FIXED** (`fix(cli): drop the repl stub — it never shipped`). The `repl` subcommand arm and its USAGE
> line are **deleted**: `chezzi repl` is now a plain *unknown command* (prints USAGE, exits 1), which is
> the honest behavior for a command that does not exist. `docs/spec.md`'s M1 row no longer claims a REPL
> shipped, and the `CLAUDE.md` Commands block no longer lists it. **No REPL was built** — the idea lives
> in `docs/future.md` (Tier 4, Ecosystem) as an explicitly-unbuilt item, which is its only correct home.

The original finding: `src/main.rs` printed *"'repl' is not implemented yet"* and exited 1, while `USAGE`
still listed `repl  Start an interactive REPL` — so for a language pitched as "Python-feel scripting" with
an ~11× faster cold start than CPython, the first thing a Python user types errored out. Building one
remains Medium: a naive v1 (accumulate lines, re-check + re-run the buffer, print the last expression) is
small, but the real work is incremental checker state, since the checker is whole-graph oriented.

### T3. No formatter
No `chezzi fmt`; no formatting provider in the LSP. (`src/fmtspec.rs` is the `{x:.2f}` mini-language —
easy to misread as a source formatter. It isn't one.) Convenience today with one author; **structural
the moment R3 lands and several people write code** — and a significant-whitespace language with no
formatter is especially exposed. Medium-large: needs a real AST→source printer with comment/blank-line
preservation (the AST doesn't carry comments today).

### T4. Test tooling is thin (but the base is honest)
`assert cond, msg`, `test fn`, `*_test.chz` discovery, `PASS/FAIL name (file:line)`, non-zero exit — a
real runner. **FAIL vs ERROR split SHIPPED (2026-07-24, §3b #1):** an `assert` failure buckets as FAIL,
any other runtime fault as ERROR (summary `P passed, F failed, E errored`). **`--max-heap=<bytes>`
memory cap SHIPPED (2026-07-24, §3b #1b):** the deterministic-in-VM runaway-allocation guard — a test
whose in-VM `Heap::live_bytes()` exceeds `N` is hard-aborted (bypassing `recover:`) and bucketed
`OVER-MEMORY` (counts as failure); `0`/omitted = OFF, so cap-off output + the dual-engine gate are
byte-identical (checks the same `lb` computed per `sweep()`, not OS RSS). The cap is **per-heap** and **M:N-engine-only** (`--max-heap`
errors if combined with `--serial`): a real runaway trips on whichever worker heap runs it. The flag is
M:N-only *by construction* to avoid a serial≠M:N divergence — the cooperative `--serial` engine shares one
heap across parent + all fibers (measures `baseline + Σ tasks`) while M:N isolates each worker (measures a
task alone), so a *concurrent* test near the boundary (allocation *split* below `N` per-fiber but summing
above) would bucket `OVER-MEMORY` on `--serial` yet pass on M:N. A cross-engine aggregate would need
non-deterministic global RSS (rejected — it would break the gate), so rather than ship the divergence the
cap is restricted to the default engine (`--serial` is the parity oracle, slated for post-freeze removal).
v1 also trips only at a GC boundary + on `Obj`-count growth — see §3b #1b. **`--timeout=<ms>`
wall-clock cap SHIPPED (2026-07-24, §3b #4):** the sibling of `--max-heap` — a test running longer than
`N` ms is hard-aborted (bypassing `recover:`) and bucketed `TIMED-OUT` (counts as failure); `0`/omitted
= OFF, so timeout-off output + the dual-engine gate are byte-identical. It rides the same `is_timed_out`
`RuntimeError` marker machinery, but the trip is observed at the **loop back-edge** (`jump_checked`) — the
hottest engine-independent checkpoint — so it catches BOTH the top-level test body (which runs outside the
fiber scheduler) and `spawn`ed-task loops. **Zero clock reads when off** (the `deadline: Option` guard
short-circuits before any `Instant::now()`; the read is throttled 1/1024 back-edges when on). **M:N-engine-
only** (`--timeout` errors with `--serial`): a wall-clock trip is non-deterministic → no serial==M:N parity.
**v1 limit (watchdog follow-up):** a test blocked in a native call (blocking syscall, `Channel.recv` with
no traffic) or spinning in loop-free infinite recursion (hits the stack guard) is NOT caught — a true
watchdog thread is the next seam. **Selection + output ergonomics SHIPPED (2026-07-24, §3b #5/#6/#7):**
`-k`/`--filter <substr>` (run a subset by name; `(K filtered out)` in the summary; zero-match = clear
failure), `--fail-fast` (stop at first non-pass, deterministic order), `--show-output` (surface a
FAILING test's stdout, default discard), `--errors=json` (machine output mirroring `check`/`run`:
`{tests:[{name,file,line?,status,duration_ms}],totals}`, suppresses human lines), `-q`/`-v` verbosity
(dots vs per-line vs per-line+timing), `--color=auto|always|never` (isatty-gated tag color), and per-
test/total timing (`-v`/json ONLY — never in default/quiet, so the byte-identity gate is untouched).
All opt-in; **default (no-flag) output is byte-identical to before**. Still missing:
fixtures/setup-teardown beyond suite hooks, coverage, benchmarks, `assert_eq` with a diff, parallel
execution across files — tracked as `docs/future.md §3b` follow-ups (CLI ergonomics).

**KNOWN-LIMIT — assert inside an FFI callback buckets ERROR, not FAIL (found 2026-07-24, WON'T-FIX).**
The FAIL/ERROR split reads `RuntimeError.is_assert`, set true only by the `Op::Assert` arm. But when a
Chezzi closure fires as a *scalar-only C callback* (`invoke_callback`, `src/vm/mod.rs`), an inner
`assert false` is laundered through `HostError` — which carries only `message` — and re-raised
`is_assert:false` at the native-return boundary (`src/vm/call.rs:210/326`), so the runner tags it ERROR.
Deterministic (both engines agree — not a parity bug), and the test still FAILS with a non-zero exit; only
the bucket *label* is wrong, on an exotic path. The clean fix (thread `is_assert` through `HostError` +
its ~10 construction sites) grows a boundary type for one cosmetic label — poor trade, so documented not
fixed. Revisit only if FFI-callback tests become common.

### T5. No debugger, no profiler, no doc generator
- **Debugger:** nothing (no breakpoints, no DAP, no stepping). What exists is post-mortem: a fault
  trace. And there is no REPL either (T2 removed the false advertisement; **no REPL was ever built**),
  so the language has **no interactive introspection of any kind** — the debug loop is "add a `print`,
  re-run". An (unbuilt) REPL would buy most of this value far more cheaply than a DAP server; it is
  tracked as a Tier-4 idea in `docs/future.md`, not as a shipped or in-progress feature.
- **Profiler:** nothing user-facing. Ironic for a project mid-perf-milestone: the VM is profiled with
  external Rust tooling, but a Chezzi *user* cannot find their own hot function. (Python: `cProfile`;
  Go: `pprof`, best in class.) A sampling counter keyed by function + a flat report is contained.
- **Doc generation:** `chezzi docs` prints *the language's own* embedded spec — it does **not** generate
  docs from a user's source. The raw material already exists (the lexer captures doc-comments; the LSP
  surfaces them on hover). Small-medium, and it's what makes third-party libraries browsable once R3
  lands (`go doc` / pkg.go.dev is a big part of why Go's ecosystem is navigable).

### T6. CI-friendliness — **not** a gap
`--errors=json` works for `check`, `run`, AND `test` (test-runner machine output SHIPPED 2026-07-24,
§3b #7 — per-test `{name,file,line?,status,duration_ms}` + totals); exit codes are correct and
deliberate (type error → 1, fault → 1, `os.exit(n)` honored, stdout write failure → 1). No gap.

## Type-system / construction (adjacent, tracked in `docs/future.md §15`)
- **Definable conversion constructors already exist** as named **static factory methods** (`fn
  Type.from_x(...) -> Type`, `Type.origin()`) — the Rust `T::from` / `T::new` idiom. No Python
  `__init__`-style overridable primary ctor is planned: `Type(...)` stays "set the fields, positionally"
  by design (`spec.md`: conversion is always visible).
- **`Convert[S]` protocol** (bound-only, partial — Phases 0–1 landed, paused) is the principled
  generalization for generic-over-conversion (`[T: Convert[S]]`). Value-position conversion + generic
  construction over the bound are deferred pending demand.
- **`FromIterable` / `Collect`** (not started): let a *user* collection plug into the `List(xs)`-style
  iterable-conversion surface so `MyColl(xs)` works like `List(xs)`. The one genuine "special ctor" gap —
  worth it only when a user collection type needs it.

## Interactive CLI — SHIPPED (the CLI streams; the buffered sink is a test harness)

**Landed.** `chezzi run` now writes each `print` straight to the process's real stdout as it happens.
A prompt appears before its `read_line`, a long-running program prints incrementally, a killed/hung
program retains what it already produced, and a spawned task's log is visible before its nursery joins
(which for a server is never). `std.io` gained `flush()` and `input(prompt)`.

**How the parity oracle survives.** The stdout sink is selected by `HostConfig::stream` (default
`false`): the lib helpers (`run_capture`/`run_file`/… and every golden + parity test) keep the BUFFERED
sink — per-task buffers, task-order flush at join, byte-identical serial-VM == M:N-VM. Only
`src/main.rs`'s `chezzi run` sets `stream = true`, and in that mode the per-task buffers simply stay
empty (the whole buffer/flush machinery degenerates to a no-op with zero scheduler edits).

**The design previously prescribed here — "stream while one task is live, buffer inside a nursery,
flush at join" — is REJECTED.** A server's nursery never joins, so its task logs would buffer for the
life of the process: the exact programs that need live logs are the ones it excludes. The deeper point
is that the task-order flush was never a *user* guarantee: the "order" is task-completion order, a
scheduler detail no correct program can lean on. Python, Go and Rust all interleave concurrent prints
nondeterministically and line-atomically, and nobody minds. A concurrent program that wants ordered
output joins and prints the collected results itself.

**The user-facing contract** (also in `stdlib.md §std.io`): one `print(...)` = one locked write →
**line-atomic** (two tasks can never garble a line; `end=""` fragments *can* interleave mid-line, like
Python); cross-task print order is **nondeterministic** on both engines; stdout and stderr are
separately locked, so a task's `print` and `eprint` may reorder relative to each other.

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
  `docs/stdlib.md` states `Err("timeout")` means *nothing arrived*. **Fix:** a fiber-latched
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
no idle worker, so the window could not open). The `abort_enlisted_scope` seam has its own scenario test
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
a cooperative time-slicer unnecessary for users. Recorded in `docs/concurrency.md` §"Cooperative contract
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

### N10. A `wait:` timer arm makes `--serial` inline-sleep instead of yielding to a runnable sibling — serial ≠ M:N — PRE-FREEZE KNOWN-LIMIT (found 2026-07-22; fix deferred to the post-freeze serial removal)
Found 2026-07-22 by the bug-hunt (channel/wait domain). A `wait:` with a live `timer(ms)` arm **and** a
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

**DECISION (2026-07-22): pre-freeze known-limit; fix deferred to the post-freeze serial removal.** The
shipping **M:N engine is correct** — user-facing impact is zero. Fixing it is a real cooperative-scheduler
change right before the JIT freeze, and the serial engine is slated for **removal** post-freeze anyway (the
oracle-layer plan in `docs/future.md` §2b). So the byte-identity tax that motivated a fix is going away.
Falsified doc claims corrected in the same commit: `docs/concurrency.md` "Observable output is identical
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
`docs/concurrency.md` — and that exception **stays**, now as a stated contract rather than a filed
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

**THE RULE (both engines, now documented in `docs/concurrency.md`):** a `defer` is never itself cancelled
and runs to completion, blocking ops (and the work it *spawns*) included. Cleanup that blocks on
**time/IO** is uninterruptible and delays the teardown for exactly as long as it takes — no cap
(`defer time.sleep_ms(10000)` in a cancelled task = a 10 s nursery join, on both engines). That is Go's
rule for a deferred fn during a panic, and it is a documented ceiling, not a bug — a cap would
re-introduce silent truncation. Cleanup that can **never** complete is REPORTED as a deadlock, never a
silent hang. **One carve-out (C5, below): on `--serial` a defer body cannot PARK.**

### N6g — OPEN (C5 family): a `defer` that `recv`s from a LIVE sibling cannot park on `--serial`
A defer body runs `guarded` (the LIFO unwind drain is host-stack state), so — exactly like a `list.map`
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
  `docs/concurrency.md` now says so.
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

> **Correction (2026-07-14 audit):** this entry used to call it "the only known serial≠M:N divergence".
> That was wrong. `std.net` is a **standing, deliberate** one — a socket op on the serial engine returns
> `Err("… requires the --parallel engine")`, so the same TCP program behaves differently on the two
> engines (see §Net). An accepted design fallback, but a divergence, and the map must say so.

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

**New v1 limit it introduces:** `read_line`/`input` are deliberately `Kind::Inline`, not blocking (the off-heap
`OffloadHost::read_line` is `unreachable!`), so a task blocked in a read now **pins an M:N core worker** —
K blocked readers occupy K workers until stdin produces lines. Previously impossible (tasks got instant
EOF). Accepted; offloading stdin reads is its own milestone.

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
  real residual risk: Miri / ASan / TSan** — Tier-1 lever #3 in [`bug-discovery.md`](bug-discovery.md). A
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
- **ureq** 2→3 — a real API rewrite of `std.request`; do as its own task when 2.x nears EOL, with
  request tests + `--parallel` verify.
- **socket2** 0.5→0.6, **libloading** 0.8→0.9 — skip until a needed feature forces it.

## Bug-hunt wave 7 (2026-07-28)

### W7-3 — a `recover:` inside a `defer` body was bypassed while the task was being cancelled (**FIXED 2026-07-28**)

**Symptom.** A cancelled task's `defer` that installs its own `recover:` lost it: the fault was not
caught and the rest of the cleanup was silently skipped. Both engines, identical.

```chezzi
parallel:
    spawn:
        defer:
            r := recover: panic("cleanup step 1 failed")
            print("recovered: {r}")
            print("CRITICAL CLEANUP")     # never printed
        _ := ch.recv()
    spawn:
        time.sleep_ms(20)
        panic("sibling-fault")
```

Half-broken: the same defer in an UNcancelled task worked, the nursery body's own defer worked, and a
`?`-propagated `Err` inside the cancelled task's defer was caught — only the fault/panic path broke.
Also reproduced with an ordinary runtime fault and with `defer cleanup()` (not a `defer:`-block artifact).

**Root cause** — `src/vm/exec.rs:1189`, the post-step `Err` funnel:
`if self.cancelled || rte.is_over_memory || rte.is_timed_out { … return Err(rte) }` bypasses the
`recover:` handler stack. `self.cancelled` is a task-wide **latch** that stays set while the cancelled
task's defers run, and the funnel was not gated on `self.deferring` — while the sibling predicate
`cancel_suppressed()` (`exec.rs:1489`) already was. Wave 6's meta-finding shape exactly: **a fix applied
to SOME arms of an N-way set**. Contract violated: concurrency.md's "A `defer` is never itself
cancelled" + syntax.md's "`recover:` catches any panic occurring transitively beneath it".

**Fix** — gate the **(a) `self.cancelled` marker ONLY**:
`let cancel_bypass = self.cancelled && !(self.deferring > 0 && caught_here);` where `caught_here` is the
already-computed `handlers.last().frame_len > base_level` test (hoisted above the `if`). A defer body
runs in its own nested `run_until`, so a handler installed INSIDE it owns the fault; one installed
OUTSIDE sits at/below `base_level` and still cannot defeat the cancel. After the defer body finishes the
pending cancel resumes travelling up — the task dies, the nursery still reports the sibling fault, `rc`
unchanged.

**(b) `is_over_memory` / (c) `is_timed_out` were deliberately LEFT ALONE** — both keep bypassing
unconditionally, so `chezzi test --max-heap` / `--timeout` aborts stay recover-proof inside a defer too.
Neither ever sets `self.cancelled` (`exec.rs:1017-1035` re-observes `over_cap()` per GC boundary,
`exec.rs:1437-1447` re-checks the deadline per back-edge), so the (a)-only gate cannot weaken them.
Requiring `caught_here` (rather than the simpler `deferring == 0`) also keeps the handler-LESS
defer-fault path byte-identical — the simple form would have re-routed it onto the `report_escaped =
true` branch, a stderr change in the N6/N6h machinery.

**Fences** — `tests/chz/spec/cancel_defer_recover_test.chz` (4 `test fn`s, serial==M:N gated): the
driver, `recover_outside_defer_cannot_defeat_cancel`,
`recover_outside_defer_cannot_catch_a_fault_raised_inside_it`, and
`faulting_defer_does_not_swallow_lifo_next` (N6d). Plus
`test_runner::recover_inside_defer_does_not_catch_timeout` pinning (b)/(c) — its load-bearing
assertion is the absent `SWALLOWED` marker, **not** the `TIMED-OUT` bucket: the outer `--timeout`
fires in the test body (`deferring == 0`), takes the unconditional bypass, and the funnel re-stamps
`.timed_out()` onto whatever emerges, so the bucket is `TimedOut` whether or not the in-defer
`recover:` swallowed the abort (adversarial-review fix — the first cut asserted only the bucket and
so could not fail).

**What the fences do NOT pin, stated honestly:** the `caught_here` conjunct in `cancel_bypass`.
Measured on the real binary, replacing `!(deferring > 0 && caught_here)` with `!(deferring > 0)`
leaves all four `test fn`s byte-identical on both engines — with no handler above `base_level` the
fault returns `Err` either way. The conjunct is kept as the **conservative** arm (it preserves the
bypass in more cases, so a cancelled task is more likely to die), not because a test discriminates
it.

### W7-2 — `Channel.close()` lost the wakeup for a `wait:`-parked fiber → spurious `deadlock:` on M:N (**FIXED 2026-07-28**)

**Symptom.** A fiber parked in a multi-arm `wait:` whose channel is `close()`d concurrently was never
woken; the deadlock detector then (correctly) reaped a genuinely unreachable fiber, so a valid program
faulted. `--serial` 0/20; `--threads=8` **6/40**, rising with parallelism.

```chezzi
a := Channel[int]()
fn w(a: Channel[int]):
    r := recover:
        wait:
            v := a.recv(): print("got", v)
    print("waiter done")
parallel:
    spawn w(a)
    spawn: a.close()
```

**The discriminating table** (what localised it — `close` is the ONLY wake path that lost it):

| waker racing the `wait:` park | failures @ `--threads=8` |
|---|---|
| `a.close()` | **6/40** |
| `a.send(1)` (recv-arm wake) | 0/40 |
| `a.recv()` (send-arm wake) | 0/40 |
| `a.trip()` | 0/40 |
| plain blocking `a.recv()` (no `wait:`) racing `close` | 0/40 |

**Root cause — NOT where the hunt's report guessed.** The report proposed "`close_wake` does not
claim/sweep the `Wait` token the way `send_wake` does". That is **false**: `send_wake` and `close_wake`
both funnel through the same `wake_bucket`, whose `ParkedEntry::Wait` arm does the claimed-CAS + sweep
identically, and both walk `wake_parent_chain` (B5). The real cause is the N-arm **gap re-check** in
`MnSched::park_wait` (`src/vm/mod.rs:2378`): its recv-arm readiness predicate was `!g.is_empty()` and
deliberately ignored `g.closed` — an in-code `parity-perf-0` note records that a previous attempt at
`closed == ready` was reverted because it live-locked (requeue → re-poll → `op_wait_poll` SKIPS the
closed arm → re-park). So a `close()` landing between `op_wait_poll`'s empty poll and `park_wait` fired
`close_wake` against a still-empty bucket, and the fiber then parked on a key nothing could ever wake.
`send`/`recv`/`trip` each leave a signal the re-check DOES read (a queued value, a free slot, the
`done_latch`), which is exactly why they never reproduced.

**Fix.** Make the arm accounting **three-way**, mirroring `op_wait_poll` instead of contradicting it:
READY (take it now) / **DEAD** (a `closed && empty && non-timer` recv arm — nothing can ever make it
ready) / LIVE. Requeue when any arm is ready **or when every arm is dead**. The all-dead requeue
terminates — the re-run `WaitPoll` hits `all_closed` and faults `wait: all channels closed`, which is
what the serial engine already does — so it does not reintroduce the `parity-perf-0` spin, and
one-dead-among-live still parks. The deadlock detector is untouched.

**Verified.** 0/60 failures at `--threads=8` (main: 3/60); a genuine all-parked nursery still faults
`deadlock:` promptly; `wait:` over all-closed channels faults identically on both engines.

### W7-4 — two sibling closures over one captured local got SEPARATE cells across the airlock (**FIXED 2026-07-29**)

```chezzi
struct Ctr:
    inc: fn() -> nil
    get: fn() -> int
fn make() -> Ctr:
    n := 0
    fn inc():
        n = n + 1
    fn get() -> int:
        return n
    return Ctr(inc, get)
fn main():
    c := make()
    c.inc()
    print(c.get())            # 1 — no airlock yet, the cell IS shared
    ch := Channel[Ctr]()
    ch.send(c)
    d := ch.recv()
    d.inc()
    print(d.get())            # was 1 — EXPECTED 2
main()
```
`chezzi check` clean. **No concurrency needed** — a `Channel` round-trip inside `main` is enough — and
identical on `--serial`, on M:N, and at `--threads=1/2/4/8`, so the parity oracle is **structurally
blind** (both engines share one serializer; there is no `src/interp/` any more). Reproduced on every
arm: `Channel.send`, `Shared`, struct-field, `.iter()` cursor, `spawn f(g, h)` args, `spawn:` block
capture, and the module-global snapshot (`to_snap`). **Another "some arms of an N-way set"** — a
module-GLOBAL aggregate reached twice already kept one identity, but a function-local **cell** did not.

**Root cause** (`src/vm/sched.rs`, `Obj::Cell` arm of `to_wire_depth`): `WireMemo` is deliberately
**back-edge-only** — a node is inserted into `memo.path` before recursing and removed on DFS exit — so a
cell revisited *off* the current DFS stack, exactly what two sibling closures produce, was re-serialized
as a fresh `WireValue::Cell` with a new `id` and `from_wire` built TWO cells. Cycles round-tripped;
shared bindings did not.

**Why this is a bug and not the documented DAG rule.** The off-path-alias-becomes-two-copies rule IS
deliberate **for DATA** (`docs/concurrency.md`): `pair := [xs, xs]` through a `Channel` gives `2 1`, a
knowing divergence from CPython's `deepcopy` memo (`2 2`). **A `Cell` is not a data node — it is a
BINDING's identity.** `docs/syntax.md` already states a write through a capture is visible in the
defining scope *and across sibling closures*, and that crossing the airlock snapshot-copies a captured
local into **an independent per-task cell** — *one* cell per binding, i.e. the sibling-sharing rule is
meant to survive inside the task. Go agrees (`f := func(){n++}; g := func()int{return n}; go func(){
f(); f(); fmt.Println(g()) }()` prints `2`).

**Fix.** `Obj::Cell` alone moves to a **persistent** `WireMemo::cells` map (never popped, so every later
reach emits `Backref`); every container arm and the closure VALUES keep the pop-on-DFS-exit `path`
discipline, leaving the data-DAG contract byte-untouched. Plus **one serialization per logical
crossing** wherever several roots cross together — otherwise the fix is undone downstream on both
engines:
- `do_spawn` — callee/receiver + all args through a new `deep_clone_all` (the old `cross_spawn_callee`
  round-trip folded into that batch; its only extra was `ensure_crossable`, which `lower_task` applies
  to the same captures with the same span).
- `do_spawn_block` — all captures in one batch.
- `lower_task` ↔ `rebuild_ready` — one memo / one rebuild map. **Serialize order must equal reconstruct
  order**, or a `Backref` hits `from_wire_memo`'s `.expect("…already-reconstructed node id")` (a panic,
  not a fault). That order is ARGS then captures: `wire_args` stays at the top of the `Call` arm (it is
  the only site applying `ensure_crossable` to spawn arguments — moving it below the callee
  classification skipped argument validation for a non-callable callee and let a capture fault pre-empt
  an arg fault), and `rebuild_ready` reconstructs a `Closure`'s args before its captures to match.
- `snapshot_modules` ↔ `fault_module` — one memo and one rebuild map **per module**.
- `to_snap_depth`'s speculative fast path ROLLS THE MEMO BACK when the attempt is discarded (restore
  `next_id`, drop every id `>= next_id`, clear `path`/`gens_on_stack`): a discarded attempt must leave
  neither a cell id nothing defines (rebuild panic) nor a `Backref` shortcut that could hide a residual
  handle from a later `has_handle`/`ensure_crossable`. Rollback, not a memo clone — the clone ran at
  EVERY node and made a module with K cell-bearing globals O(M·K).
- **Cross-heap STORES serialize with `WireMemo::elem_split`** (`to_wire_crossable`, the single
  chokepoint every store routes through). `RwShared`'s zero-copy read views are the one place ONE
  serialize memo is drained by MANY independent `from_wire`s, so the persistent cell memo made a
  `Backref` legal BETWEEN SIBLING pieces of a stored wire for the first time and
  `RwShared([inc, get]).at(1)` hit that `.expect` — a host PANIC, no concurrency, both engines. Fix:
  a stored wire re-emits a cell's FULL definition once per **depth-1 subtree** (same id), and
  `from_wire_memo` DEDUPES a repeated definition by id. Every drained piece is therefore
  self-contained, and a whole-value rebuild still ties every reference to one cell. Cost is a little
  wire size (only for a cell reached from 2+ depth-1 subtrees), and `src/vm/netio.rs` keeps main's
  plain `from_wire` per piece. **The rejected alternative** (round 2) was to resolve a piece's backrefs
  by RE-READING `core.v` to rebuild the whole container: two separate read guards → a concurrent `set`
  in the window resolved the piece against an unrelated serialization (`.expect` abort, or a
  `CellLoad on a non-cell object` wrong-node abort, M:N-only = parity-blind), and it was O(n²) — a
  4000-element `for_each` went 0.011 s → 3.7 s, 12000 → 34 s.

**Intended contract flip:** `airlock_aliased_closure_stays_independent` (`[bump, bump]`) →
`airlock_aliased_closure_shares_its_binding`, `1` → `2`. The closure *values* are still two independent
copies; the one *binding* is now one cell.

**Where the rule STOPS (checked, not a residual).** Identity is preserved within ONE crossing, never
BETWEEN crossings. Two separate tasks over one local — two `parallel: spawn:` blocks, or two
`Executor.submit` calls — still each snapshot the binding independently, and the parent sees neither
write. That is the documented F1 per-task isolation (`syntax.md` rule 2), not a leftover arm of this
bug; it is now fenced by `separate_tasks_each_get_their_own_binding` so a future "make the cell memo
`Vm`-lived" over-reach goes red. A single `Executor.submit` whose one closure holds both sides of a
pair WAS the bug and is fixed (`0` → `2`).

**Verified.** `tests/chz/spec/airlock_shared_binding_test.chz` — 15 tests (7 arms, the `RwShared`-views
regression, a view run CONCURRENTLY with a writer, the spawn args-before-callee fault-ordering pin, the
discarded-snapshot-walk rollback fence, 4 fences), green on both engines under
`chz_suite_passes_both_engines`; thread sweep `1/2/4/8`; the full 26-test `airlock_` panel (cycles on
every container arm, recursive/mutually-recursive local `fn`, the generator `reference cycle` reject,
the depth cap, handle `Arc` identity, the module-global inert-`Nil` generator) unchanged; a new
`airlock_cross_arg_data_alias_stays_independent` fence and a new
`airlock_module_global_shared_binding_survives_gc_stress` rooting lock, plus
`rwshared_view_over_shared_bindings_is_not_quadratic` (a coarse cliff detector: 10.5 s pre-fix debug,
0.03 s after). **Perf** — `benches/run.chz` flat on all 9 (no airlock there); 100k `Channel.send`/`recv`
round-trips 127 ms → 124 ms; 20k-`spawn` storm 221 ms → 217 ms; `RwShared.for_each` over 4000
sibling-binding closures main 0.011 s → round-2 branch 3.7 s → 0.012 s; snapshot stress (400
module-global closures over distinct cells × 1000 nurseries) main 1.084 s → memo-clone 1.243 s (+15%)
→ rollback 1.110 s (+2.4% vs main).

**Residual ceilings** — all the same shape: TWO INDEPENDENT SERIALIZATIONS reach one cell; identity is
per serialization. **Three of the four are now resolved (2026-08-05)** — see
[§W7-4a/b — the snapshot path](#w7-4ab--the-module-snapshot-path-keeps-one-cell-per-binding-fixed-2026-08-05)
and [§W7-4c](#w7-4c--a-tasks-own-captures-and-its-module-snapshot-are-one-binding-fixed-2026-08-06):
- ~~**W7-4a**~~ — **FIXED**: one `WireMemo` spans the whole snapshot and one `Vm`-lived rebuild map
  (`Vm::snapshot_rebuild`) spans every lazy module fault, so two globals in DIFFERENT modules over one
  shared cell arrive as one cell. `0` → `2`, matching CPython and Go.
- ~~**W7-4b**~~ — **FIXED**: `SnapValue` gained `Cell { id, inner }` + `Backref(id)`, minted from the
  same memo and drained by the same rebuild map, so a cell on the snapshot SLOW arm keeps its identity
  too. `1` → `3`, matching CPython. (The filed premise was stale — `Native`/`Cffi` cross by value now,
  so only `Obj::Module` still forces that arm.)
- ~~**W7-4c**~~ — **FIXED 2026-08-06**: ONE TASK reached through TWO serializations now gets ONE
  binding. `0` → `2`, matching CPython. Four mechanisms (snapshot cell registry + monotonic ids, a
  seeded spawn-time clone, a per-task id carry, and the W6-2 pin moved ahead of the clone) — and the
  fence `module_global_plus_local_capture_still_split` flipped to
  `..._shares_its_binding`. **One thing stays open: a +10.2% cost on a spawn storm that gains nothing
  from it** — see the section below.
- ~~**W7-4d**~~ — **CLOSED as not-a-bug**: an `RwShared` COPY-OUT VIEW is per-piece independent:
  `at`/`for_each`/`fold`/`get_key`/`has`/`for_each_entry`/`fold_entries` rebuild one piece per step, so
  two sibling closures pulled out separately do not share their binding (two `at()` calls ARE two
  crossings — they never could). A whole-container `get()`/`read()`, and `slice` (one call returning a
  container), ARE one crossing and do share. Inherent to a copy-out API, never a residual of the fix.

### W7-4a/b — the module-snapshot path keeps one cell per binding (**FIXED 2026-08-05**)

```chezzi
# k.chz                       # l.chz            # main.chz
struct Ctr:                   import k           import k
    inc: fn() -> nil          GI := k.C.inc      import l
    get: fn() -> int                             GG := k.C.get
fn make() -> Ctr:                                fn main():
    n := 0                                           r := Channel[int]()
    fn inc():                                        parallel:
        n = n + 1                                        spawn:
    fn get() -> int:                                         l.GI()
        return n                                             l.GI()
    return Ctr(inc, get)                                     r.send(GG())
C := make()                                          print(r.recv())   # was 0 — EXPECTED 2
```

| | Chezzi (before) | Chezzi (after) | CPython | Go |
|---|---|---|---|---|
| **a** — cross-module globals over one cell | `0` | **`2`** | `2` | `2` |
| **b** — `p := [k]` (a cell holding a module) | `1` | **`3`** | `3` | — |

The references are paired programs, not reasoning: CPython `pk.py`/`pl.py`/`pmain.py` with
`threading.Thread`, and Go packages `k`/`l` + `main` with a goroutine. Both share the binding, so
Chezzi's split was drift, not F1 isolation — F1 says the PARENT must not see the task's write, and it
still does not.

**Root cause (a)** — `snapshot_modules` built one `WireMemo` **per module**, and `fault_module` one
rebuild map **per module**. A cell reached from globals in two modules therefore got a fresh id in each
and rebuilt twice. **Fix**: hoist the memo out of the loop (`cells`/`next_id` persist) and clear only
`emitted` per module, so every module stays **self-contained** — it re-emits a shared cell's FULL
definition under the SAME id and `from_wire_memo`'s first-wins dedupe ties the second to the first.
That is what makes LAZY fault order irrelevant, and a module the task never touches free. The cost is
wire size, only for a cell reached from 2+ modules — the same trade `elem_split` already makes for
`RwShared` stores. The rebuild map moved to `Vm::snapshot_rebuild`, swapped with the view and rooted.

**Root cause (b)** — `SnapValue::Cell` carried no id. **Fix**: `Cell { id, inner }` + `Backref(id)`,
minted from the same shared `WireMemo` as the wire arms, so a binding reached down BOTH the fast
(`SnapValue::Wire(WireValue::Cell)`) and slow (`SnapValue::Cell`) paths keeps one identity; `replay_snap`
dedupes first-wins into the same map `from_wire_memo` uses. A dangling `Backref` degrades to `nil` and
flags `wire_backref_missing` (W7-11's soft miss), never an `.expect`.

**A SECOND fix fell out of (b), unplanned and verified against the pre-fix binary.** A recursive local
`fn` whose captures embed a module used to abort the whole spawn:

```chezzi
import k                      # k.chz: V := 41
fn make() -> fn(int) -> int:
    m := k                    # a captured local holding a MODULE → the closure fails the fast lane
    fn down(n: int) -> int:   # …and `down` captures its own cell → a self-cell CYCLE on the slow arm
        if n <= 0:
            return m.V
        return down(n - 1)
    return down
G := make()
# spawn: r.send(G(3))
#   before → runtime error: maximum structural depth (10000) exceeded (cyclic data structure?)  [rc=1]
#   after  → 41          CPython (`m = bk` + the same recursive `down`) → 41
```
The `Obj::Closure` slow arm's comment said the depth-cap walk "rejects cleanly … identity preservation
is a wire-only concern". Clean it was; correct it was not — the wire path had round-tripped that exact
cycle for a year via `Backref`, and only the snapshot path faulted. Giving `SnapValue::Cell` an id
terminates the walk at the second reach, so the two paths agree. Fenced by
`airlock_handle_bearing_recursive_local_fn_round_trips`. **A "clean reject" note is not evidence the
reject is right** — this one was a `to_wire`-vs-`to_snap` divergence hiding behind a tidy error
message, in the same family as `docs/gaps.md` W7-12's "correctness outranks engine agreement".

**ADVERSARIAL REVIEW CAUGHT A LIVE REGRESSION THE WHOLE GREEN GATE MISSED.** Both prosecutors,
independently, found the same critical bug in the (a) fix — and it was a host PANIC that diverged by
engine, on a program the pre-fix binary ran fine:

```chezzi
# k.chz  = the Ctr(inc, get) pair over a local `n := 7`
import k
H  := (k, k.C.get)      # embeds a MODULE → fails to_snap's !has_handle() fast lane
GG := k.C.get           # …and this global then emits a Backref with no definition
# spawn: r.send(GG())
#   base cbca2561 → 7            M:N after the (a) fix → PANIC: CellLoad on a non-handle value
#                                --serial after the (a) fix → 7      (parity-DIVERGENT)
```
`try_wire_speculative` rolled `emitted` back with `retain(|id, _| *id < mint_from)`. That was a
COMPLETE undo only while every id in the memo had been minted by the current module's own walk —
"below the watermark" meant "really emitted". Making the memo span modules (the (a) fix) silently
broke that premise: a discarded attempt can mark an id minted in an EARLIER module, which is *below*
the watermark, so the rollback kept a marking the thrown-away encoding invented. Fixed with an
`emit_undo` journal (`(id, the entry it replaced)`, recorded only while `speculating`) replayed
newest-first on discard. Order-dependent: move `H` below `GG` and it prints `7` either way. Fenced by
`airlock_discarded_wire_attempt_does_not_forge_a_backref`, verified to reproduce the exact panic with
the journal removed.

**The lesson is about WIDENING A SCOPE, and it is the same shape as
`lossy-decode-blinds-a-comparison-oracle`:** when a change widens what a piece of state spans, every
*existing* consumer of that state carries an unstated assumption about the old, narrower scope. The
watermark rollback was correct code that became wrong without being touched. Grep the state's other
readers in the same commit — `cells`, `next_id`, `path` and `gens_on_stack` were all audited and are
fine; `emitted` was the one that was not, and 3830 green tests plus a two-engine chz suite plus a
thread sweep all missed it because no test had a handle-bearing global positioned BEFORE a
cross-module cell reference.

Review also found (and fixed) two real secondary defects: `replay_snap`'s `Backref` miss set
`wire_backref_missing` with **no consumer**, so a snapshot miss leaked into the next unrelated
`from_wire` caller's `debug_assert` — `fault_module` now owns the flag around its replay; and
`Vm::snapshot_rebuild` retained EVERY identity-preserved node (`List`/`Map`/`Set`/`Struct`/`Tuple`/
`Closure`), not just cells, making the module-global object graph immortal for the fiber's life
(a `--max-heap` regression for a task that reassigns a big global) — `fault_module` now prunes to
cells, which is sound because only a cell can be back-referenced across modules (containers live in
`path`, which pops on DFS exit).

**Two lessons, both about PRICING a filed residual:**
1. **The predicted cost was wrong in the expensive direction.** W7-4a's ceiling comment said closing it
   needs `Vm`-lived rebuild state "kept across GC-visible points" — implying delicate rooting. It does
   need the `Vm`-lived map, but the rooting turned out to be **belt-and-braces**:
   `airlock_cross_module_shared_binding_is_one_cell` still passes with the `collect` root line deleted
   (measured), because every entry is also reachable from the global it was just `module_define`d into.
   The root line is kept anyway — cheap, and the map now outlives a single fault.
2. **W7-4b's premise had gone stale under it.** It was filed as "a residual `Module`/`Native`/`Cffi`
   handle", but `Native`/`Cffi` cross BY VALUE now — only `Obj::Module` still sets `has_handle`, and the
   code calls that "source-unreachable, defensive only". That reads as unreachable, so the residual
   looks unpriceable. It is reachable: a module IS bindable to a local (`m := k`), so a cell over `[k]`
   lands on the slow arm from ordinary source. **Re-derive a residual's premise against today's code
   before trusting its price tag.**

**Verified.** `airlock_cross_module_shared_binding_is_one_cell` + `airlock_handle_bearing_cell_keeps_one_binding`
(each: serial, M:N, and a MULTI-FILE gc-stress run via the new `run_file_stress` — `run_capture_stress`
is single-source and cannot reach the lazy per-module fault path). Full `cargo test --lib` 3832 green;
`chezzi test tests/chz/` 297/297 on both engines; the 29-test `airlock_` panel green; thread sweep
`1/2/4/8` × 25 runs on the cross-module repro, 0 wrong. **Perf** — the W7-4 snapshot stress (400
module-global closures over distinct cells × 1000 nurseries) main 2.585 s → 2.573 s (flat, within run
noise of ±0.06 s).

### W7-4c — a task's own captures and its module snapshot are ONE binding (**FIXED 2026-08-06**)

```chezzi
C := make()            # the Ctr(inc, get) pair over a factory-local `n`
GI := C.inc            # a module GLOBAL  -> crosses via the module snapshot
fn main():
    gg := C.get        # a captured LOCAL -> crosses via the spawn-time clone
    parallel:
        spawn:
            GI(); GI(); r.send(gg())
    print(r.recv())    # was 0 — CPython measures 2
```

The trace, and why the W7-4a fix did not transfer:

```
parent cell N --deep_clone_all--> N' (parent heap) --lower_task--> id --rebuild_ready--> N''  (worker)
parent cell N --to_snap--------------------------> snapshot id X ----------> fault_module --> N''' (worker)
```

**Four mechanisms**, all needed:
1. `Vm::snapshot_cells` + a MONOTONIC `snapshot_next_id` — the registry of every cell the cached
   snapshot numbered, keyed by `GcRef`. Its keys are GC roots, load-bearing: an unrooted key could be
   swept and its slot recycled to a different cell, silently merging two bindings. Monotonic ids mean a
   snapshot renumbered between `spawn` and preparation makes a stale id **miss** (degrade) rather than
   **collide** (wrong answer).
2. `deep_clone_all` seeds from the registry (shared by `Arc`, never copied — a per-spawn O(cells) clone
   is the O(M·K) shape W7-4 already rejected) and reports the CLONE cells' ids on the task.
3. `lower_task` seeds from those, so the clone serializes under the snapshot's id; `rebuild_ready`
   joins the one `Vm`-lived rebuild map `fault_module` drains.
4. The **W6-2 pin instant moves ahead of the clone** (`pin_snapshot`). It has to: `deep_clone_all` ran
   BEFORE `register_task`'s `ensure_snapshot`, so the first spawn of a view cloned its cells before any
   id existed. No user code runs between, so pinned VALUES are unchanged; only which fault wins when
   both the snapshot build and the crossing are non-viable.

**ADVERSARIAL REVIEW CAUGHT TWO CRITICALS, and the first is the interesting one.**

**(a) A shared identity forces a shared VALUE — and the two crossings held different ones.**
`from_wire_memo`'s `Cell` arm is FIRST-WINS. A write THROUGH a cell does not drop `snapshot_memo`
(only a module-SLOT write does), so a cached snapshot can carry a stale cell while the task's clone
carries the value at its own `spawn`:

```chezzi
parallel:
    spawn: pass          # builds+caches the snapshot, cell = 0
    I()                  # a write THROUGH the cell — cache NOT dropped
    spawn: r.send(gg())  # the clone carries 1
#  serial 0  |  M:N 1  |  CPython 1  |  serial's own pre-W7-4c answer 1
```
Serial eager-faults every module before rebuilding the task; M:N rebuilds first and faults lazily — so
the engines picked different winners. Fixed by rebuilding the task's crossing FIRST on both (the clone
is the correct value: a task sees the binding as of its own spawn). Fenced by
`airlock_shared_cell_takes_the_spawn_time_value_on_both_engines`. **Lesson: unifying identity across
two serializations silently unifies their VALUES too, and "same binding" does not imply "same
instant". Before merging two copies of anything, ask what happens when they disagree** — here one was
a cached snapshot that no rule invalidates on a cell write.

**(b) The monotonic-counter guarantee was split from the thing it guarded.** `snapshot_cells` went into
the `FiberCtx` swap group; `snapshot_next_id` did not. Every M:N shell starts at `0` and one shell
drains fibers from several scopes, so a registry numbered on shell A resuming on shell B would re-mint
ids its own entries already use — two unrelated bindings merged, silently. The counter now travels
with the registry, and a `debug_assert` in `deep_clone_all` re-checks the invariant.

**Verified.** `0` → `2` on both engines; the full `airlock_` panel (32 Rust + 15 chz) green, including
`separate_tasks_each_get_their_own_binding` (two tasks must still split) and
`airlock_cross_arg_data_alias_stays_independent` (the data-DAG contract); `cargo test --lib` 3833;
`chezzi test tests/chz/` 297/297 both engines; thread sweep `1/2/4/8` × 25, 0 wrong. The fence
`module_global_plus_local_capture_still_split` FLIPPED to
`module_global_plus_local_capture_shares_its_binding` (`0` → `2`) — an intended contract change.

**OPEN — perf cost, not yet closed.** A 120k-`spawn` storm that holds **no** module-global cells (so it
gains nothing) runs **+10.2%** (0.919 s → 1.013 s, interleaved A/B, medians of 7). Snapshot stress is
flat (+0.2%), `loop` flat. Three optimisations are already in (share the registry by `Arc`; skip the
report scan when the registry is empty; keep the throwaway rebuild map when a task has no shared ids —
that one alone recovered ~5%). The residue is spread across the task pipeline (`QueuedTask` grew a
`Vec`, `deep_clone_all` returns a tuple, extra per-spawn moves) and was not localised further: no
`perf` on the box, and a hand bisect was invalid because the probe allocated a `String` per spawn.
**Next step: profile the spawn path properly and recover it.**

## Session log — 2026-07-28 (bug-hunt wave 7 — the P2 tier: 3 findings; ALL THREE FIXED — W7-9 + W7-10 2026-07-30, W7-8 2026-07-31)

These three came out of the same wave-7 hunt as W7-1…W7-7 and were filed rather than rushed, each
needing a design decision or a seam change bigger than a patch. **Two have since been fixed
(2026-07-30): W7-9** (the `Reader` carry) **and W7-10** (the csv bare-quote policy call — CPython
"keep it literally"), and **W7-8 followed 2026-07-31** — it did need the new `bytes`-carrying path
seam, which landed as the `PathLike` protocol + `path.Path` type. All three were **re-verified on `main` after the
wave-7 fixes landed** (2026-07-28), both engines identical, `chezzi check` clean on every repro. None
is a serial≠M:N divergence — the parity oracle is blind to all three, which is why they needed a
differential against CPython/Go to surface.

### W7-8 — `fs`/`os` hand back a LOSSILY-DECODED path that does not open (**FIXED 2026-07-31**)

`fs.list_dir` / `fs.walk` / `fs.glob` / `fs.canonicalize` and `os.getcwd()` run the OS bytes through
`to_string_lossy`, so a non-UTF-8 name comes back with `U+FFFD` substituted — a path that names
nothing. The program gets no diagnostic; the next `exists`/`open` on that name simply fails.

```chezzi
import std.fs
fn main():
    match fs.list_dir("/tmp/bd"):        # dir holds b"A\xffB.txt" and "ok.txt"
        Ok(xs):
            for n in xs:
                print(str(n.encode()), "exists =", str(fs.exists("/tmp/bd/" + n)))
        Err(e): print(e.message())
main()
```
```
b'A\xef\xbf\xbdB.txt' exists = false      <- U+FFFD; the path does not exist
b'ok.txt' exists = true
```
`io.read_file` on it → `Err(… No such file or directory)`. Same for a non-UTF-8 cwd
(`cwd = Ok(/tmp/cw�dir)`, `fs.exists(cwd) = false`). **Python** hands back the exact bytes
(`os.listdir(b'…')`, `os.getcwdb()`).

**Sites (corrected — the original list was stale on two counts):** `src/native/fs.rs:37,144,160,239`
were the four production decodes; **`fs.rs:469` is a `#[cfg(test)]` helper host, not a bug**, and
**`os.rs:63` is `hostname`'s decode** (a display string, correctly lossy) — `getcwd`'s decode actually
lived in `Host::os_getcwd` at `src/native/mod.rs:467`, whose return type was `String`.

**FIXED 2026-07-31 — the `bytes`-carrying path seam landed, as `PathLike` + `path.Path`.**
Design doc: `~/.claude/plans/2026-07-31-path-pathlike-design.md`.

* **INPUT** — a new reserved universe protocol `PathLike` (sole method `as_path(self) -> bytes`), the
  20th. `str`/`bytes`/`bytearray` satisfy it **intrinsically** (three grant rows in
  `INTRINSIC_PROTO_METHODS` + a miss-only `("as_path", 0)` arm in `Vm::intrinsic_proto_method`);
  `path.Path` satisfies it structurally. Every path-taking fn in `std.fs`/`std.io`/`std.os`/`std.path`
  takes one, so `fs.exists("x")` still compiles with a bare `str` literal — **not a breaking change**.
* **OUTPUT** — `path.Path`, an **ordinary Chezzi struct** over `raw: bytes` (deliberately not a
  `native struct`: no `NativeRet::Struct`, no fourth hand-maintained positional layout copy).
  DISPLAY and CONVERSION are separate: `p.str()` is lossy and never faults (`Stringable`), `p.decode()`
  is exact with a recoverable fault, `p.bytes()` is raw. Rust makes the same split (`Path` has no
  `Display`). `os.getcwd() -> Result[path.Path]` — a CONCRETE return type, so the erasure blocker that
  killed `os.getcwd[bytes]()` (type args are erased before `Vm::call_native`) never arises.
* **SEAM** — each path-taking native is `_`-prefixed and typed `bytes` (`_exists`, `_list_dir`,
  `_getcwd`, …); the public name is a bodied pure-Chezzi wrapper doing `_native(p.as_path())`. All
  four production decodes are byte-exact (`OsStrExt`), and `glob`'s matcher runs over `&[u8]`. Lossy
  rendering survives ONLY in human-facing error text (`Path::display()`), which is the ratified
  `p.str()` semantics.
* **`std.path`** — all 10 lexical helpers moved from `str -> str` to `PathLike -> Path` (option A in
  the design doc), so a non-UTF-8 name survives `basename`/`join`/`normalize` too. Ops chain and you
  convert once at the end. `join` — the one helper whose path sits in a CONTAINER — is
  `[T](parts: List[T]) -> Path where T: PathLike`, **not** `List[PathLike]`: containers are invariant
  (unchanged by this work), so the `List[PathLike]` spelling would have been callable with a list
  LITERAL and nothing else. See the second-panel findings below.
* **Two enabling front-end defects had to be fixed first** (both of the recorded
  checker-superset-of-compiler class, both latent on main):
  1. `Compiler::collect_globals` never reserved a slot for a `native fn`, so a **bodied fn in a native
     module could not call a native sibling** — it panicked `global '_exists' has no slot`.
  2. the checker's native-module arm bound a module's imports only INSIDE its `has_bodied` branch, i.e.
     AFTER `harvest_native_module` had already resolved every signature — so a native module's
     **signatures could not name a type from a module it imports** (`unknown module 'path'`).
  A third surfaced during the port: `Vm::do_method_call`'s Module arm called `do_call`
  unconditionally, which FLATTENS the callee frame for the running dispatch loop — correct only while
  every module member was a native. A `defer fs.remove_file(p)` (re-entrant, `NO_IC`, no running loop)
  then ran off the end of the proto. It now takes the synchronous `invoke_value` path when
  `ic == NO_IC`, exactly like the struct/enum arms.

**Two findings from the manual adversarial panel, fixed in the same commit:**
* `os.temp_dir()` was still lossily decoded (`src/native/os.rs`, `.display().to_string()`) — a
  path-RETURNING API the original W7-8 report never named, through which a `U+FFFD` path stayed
  constructible. Now `-> path.Path` over raw bytes, so the "no unswept member" claim above is true.
  (`os.home_dir()` deliberately stays `Option[str]`: it reads the HostConfig env map, which is the
  documented, separately-scoped lossy argv/env surface.)
* porting `glob`'s matcher to bytes had silently made `?` count one BYTE rather than one Unicode
  scalar, so `glob("a?c")` would have stopped matching `aéc` — a drift from Python `fnmatch` / Go
  `filepath.Match`. `?` now consumes one full UTF-8 scalar wherever the name is valid UTF-8, falling
  back to one byte only where no valid sequence starts (the only rule defined there at all).

**Three findings from the SECOND adversarial panel, fixed on the same branch:**
* `path.join(parts: List[PathLike])` was **uncallable with any list variable** — container invariance
  (which this work explicitly preserves) rejects `List[str] -> List[PathLike]` *and*
  `List[path.Path] -> List[PathLike]`, so only an inline literal type-checked. A hard regression
  against main's `List[str]` (`path.join(s.split("/"))`, `path.join(xs)` both stopped compiling), and
  the new API did not compose with its own output (`fs.list_dir` hands back `List[path.Path]`). The
  whole test table used literals, so the suite was structurally blind. Now generic over the element
  type with a `PathLike` bound; invariance is untouched and fenced both directions in
  `pathlike_grant_does_not_widen_container_invariance`, and `t_join_of_variables` +
  `list_dir_round_trips_a_non_utf8_name` exercise `List[str]`/`List[bytes]`/`List[Path]` variables.
* Both `glob` doc sites (`docs/stdlib.md`, `std/fs.chz`) still stated `?` counts one **byte** — the
  behavior the panel finding above had already reversed, so the published contract contradicted the
  code and its own unit test. Corrected, and pinned end-to-end by `glob_question_matches_one_scalar`
  on a real `aéc.txt`.
* The byte-exact `std.path` rewrite cost **2.70×** against main's native-`str` module (`bytes` has no
  `split`/`join`/`+`, so the first cut ran per-BYTE `bytearray.push` loops in the VM) and landed with
  no `docs/benchmarks.md` entry. `bytearray.extend` + one shared `_last_idx` backwards scan bring it
  to **1.73× vs main (1.56× faster than the first cut)**; measured and recorded. The residual is
  `_split`'s per-byte loop — a native `bytes.split(sep)` is the named upgrade path.

**Verified by hand on the release binary, BOTH engines, byte-identical** (`b"A\xffB.txt"` fixture):
`list_dir`/`walk`/`glob`/`canonicalize` all return the exact bytes and `fs.exists` on the recovered
name is **true** (it was **false** on the pre-fix binary, which returned `b'A\xef\xbf\xbdB.txt'`).
A non-UTF-8 cwd likewise round-trips through `os.getcwd()`.

### W7-9 — `Reader.read_line`'s non-UTF-8 fault CONSUMES the line it could not decode (**FIXED 2026-07-30**)

The fault is recoverable, but the bytes are gone: the `read_bytes` the error message itself recommends
returns the *next* line, not the one that failed.

```chezzi
import std.io                    # /tmp/bin.dat == b"line1\nA\xffB\nline3\n"
fn main():
    match io.open("/tmp/bin.dat"):
        Ok(r):
            print("l1 =", str(r.read_line()))
            x := recover: r.read_line()
            match x:
                Ok(l): print("l2 =", str(l))
                Err(e): print("l2 FAULT:", e.message())
            print("rest =", str(r.read_bytes(100)))
        Err(e): print(e.message())
main()
```
```
l1 = Some(line1)
l2 FAULT: stream did not contain valid UTF-8 — read binary files with Reader.read_bytes
rest = Ok(b'line3\n')          <- b"A\xffB\n" is gone forever
```
**Why it matters:** it breaches the rule ratified with B1/R1 and quoted in the W6-4 entry — *"a
recoverable `Err` that silently drops already-received payload would just be a different flavour of the
corruption B1 fixes."* `Socket.read` keeps undecodable bytes in `SocketCore::carry` precisely so
`read_bytes` can recover them; `Reader` has no carry. Same "advice that doesn't work" shape as W6-18.
`docs/stdlib.md` ("a clean **fault** pointing at `read_bytes`") implies recovery is possible.

**FIXED 2026-07-30.** `ReaderCore` grew a `carry: Mutex<Vec<u8>>` mirroring `SocketCore::carry` (same
`carry`-OUTER/`inner`-INNER lock order, one critical section per read). The root cause was not a
missing buffer but the *read shape*: `BufRead::read_line(&mut String)` consumes the line off the
`BufReader` and only then returns `InvalidData`, with the bytes already dropped — so `read_line` now
does `read_until(b'\n')` + `String::from_utf8`, and on a decode failure stashes the RAW line
(terminator included) in the carry before faulting. The fault message and the terminator-strip are
byte-for-byte unchanged. `read_bytes` drains a pending carry FIRST without touching the fd (a
carry-only *short* read, the `socket_read_bytes` shape at `netio.rs:470`); `close` takes the carry lock
first, clears it, and drops the fd; every read arm checks `inner.is_none()` BEFORE serving the carry,
so a carry can neither leak past `close` nor resurrect after EOF. **All FOUR read paths** were taught
about it — the three native arms (`read_line`, `read_bytes`, `close`; `reader_method` in
`src/vm/fileio.rs` is the whole Reader dispatch, there is no `read_all`) plus the bodied pure-Chezzi
generator `lines()` (`std/io.chz`), which inherits carry and stickiness for free by looping
`read_line`. Two deliberate consequences, both documented: the fault is **sticky** (a re-read
re-decodes the same bytes and re-faults, never skips — a `lines()` loop must drain with `read_bytes`
or `close` to move on, exactly the ratified `Socket.read` behaviour) and **self-healing** (a partial
drain leaves a remainder that, if it decodes, becomes the next line). New observed output, identical
on `run` and `run --serial`:
```
l1 = Some(line1)
l2 FAULT: stream did not contain valid UTF-8 — read binary files with Reader.read_bytes
rest = Ok(b'A\xffB\n')     <- was Ok(b'line3\n'); the refused line, byte-exact
then = Ok(b'line3\n')
```
Fenced by `tests/chz/stdlib/io_reader_carry_test.chz` (6 `test fn`s: non-destructive recovery,
stickiness, partial-drain resume, the `lines()` arm, close-discards-the-carry, EOF-does-not-resurrect).

### W7-10 — `csv.parse` silently DELETES a bare `"` inside an unquoted field (**FIXED 2026-07-30**)

```chezzi
import std.csv
fn t(s: str):
    print(str(s.encode()), "=>", str(csv.parse(s)))
fn main():
    t("a,b\"c")
    t("a,b\"c\"d")
    t("a,b\"\"c")
main()
```
```
b'a,b"c'   => [[a, bc]]
b'a,b"c"d' => [[a, bcd]]
b'a,b""c'  => [[a, bc]]
```
**CPython** `csv.reader` keeps them literally (`['a','b"c']`, `['a','b"c"d']`, `['a','b""c']`);
**Go** `encoding/csv` errors (`bare " in non-quoted-field`). Chezzi picks a silent third answer.
The hole is narrow — the quote-*starts*-the-field cases (`a,"b"c` → `bc`, `"a"b,c` → `ab`) match
CPython exactly. `docs/stdlib.md` says only "RFC 4180 quote state machine" and never mentions bare
quotes.
**FIXED 2026-07-30 — policy: CPython.** A `"` opens a quoted field ONLY at FIELD START; anywhere else
it is an ordinary character kept literally. Go's `bare " in non-quoted-field` error was rejected
precisely because `parse -> List[List[str]]` has no error channel, and adding one is a signature
change. The patch is a per-FIELD `field_start` flag in `std/csv.chz`'s state machine (the record-level
`started` is NOT reusable — a `,` sets it too, and a `field.len() == 0` heuristic gets `""x"y` wrong):
the quote-opens branch is gated on `and field_start`, and a non-field-start quote falls through the
existing elif chain into the ordinary-char `else`, which already pushes the char and sets
`started = true`. The pre-collected `chars: List[str]` + `field: List[str]` O(n) structure is
untouched (no `text[i:i+1]` per char). New output, identical on `run` and `run --serial`:
```
b'a,b"c'   => [[a, b"c]]      b'a,"b"c' => [[a, bc]]     <- fences, UNCHANGED
b'a,b"c"d' => [[a, b"c"d]]    b'"a"b,c' => [[ab, c]]
b'a,b""c'  => [[a, b""c]]     <- TWO literal quotes; `""` collapses only INSIDE a quoted field
```
Fenced by `tests/chz/stdlib/csv_bare_quote_test.chz` (4 `test fn`s: the three bare-quote cases, both
quote-starts-the-field regression fences, RFC 4180 embedded comma/newline/`""`-inside-a-quoted-field,
and the total round-trip).

## Session log — 2026-08-01 (Executor drain milestone: W7-5 + W7-5c FIXED; W7-5b FIXED 2026-08-03 by eager execution)

### W7-5 — the M:N `Executor` drain did not abort remaining jobs after a fault, and dropped a completed job's result (**FIXED 2026-08-01**)

**Decision: run every queued job; raise the lowest-submission-index fault.** Two prior fix attempts
were prosecuted and rejected (see `PROGRESS.md`'s superseded note and the safe-direction observation
below on why one of the two rejections was itself measuring the wrong thing). The landed fix keeps
run-all — an ordinary job fault no longer aborts its siblings — which matches three independent
reference models, none of which abort siblings by default: Python's `ThreadPoolExecutor` (a fault in
one submitted job does not cancel the others; `as_completed`/`result()` surfaces each job's own
outcome), Java's `ExecutorService` (`submit` isolates each task's exception behind its own `Future`;
`shutdown()`+`awaitTermination()` does not abort in-flight work on a sibling's failure), and Go's
`errgroup.Group` (the default `Group` — as opposed to `WithContext`'s opt-in cancellation — lets every
goroutine run to completion and returns the first non-nil error). Early-stop is available but is now
**opt-in in the caller**, via `std.cancel.Token` threaded through the closures — the same split Go
itself uses (`errgroup.WithContext` layers cancellation ON TOP of the plain `Group`, it is not the
default).

**What both prior (rejected) attempts got wrong: conflating the drain's per-drain cancel flag with the
run-all decision.** The cancel flag is not one on/off switch — it has to split into two different
questions: "should an ordinary sibling fault stop other jobs" (no, per the decision above) vs "should a
HARD halt still stop other jobs" (yes, unconditionally — a `--max-heap`/`--timeout` abort, or a fault
raised while stdout is dead, must stay un-swallowable, or `chezzi run x.chz | head -1` spins the whole
queue instead of exiting promptly). **Superseded in part by W7-5d (2026-08-05, `:5188`):** the
dead-stdout half of that "yes" was wrong. `| head -1` promptness never needed it — `stream_halt`
faults each printing job at its own `print`, so the queue is bounded by job count — and the term was a
process-global read that reclassified every fault in the process. Only the two resource caps are hard
halts now. The fix keeps the cancel flag exactly for the second case
(`executor_hard_halt`, gating `trip_cancel()` in `ReadyWorker::run_outcome`) and removes it for the
first — the earlier attempts either kept the flag for both (matching the old abort-on-any-fault
behavior, defeating run-all) or removed it for both (defeating the hard-halt kill switch). `os.exit`
(`pending_exit`) is a third, separate case — an unconditional hard halt regardless of the
`executor_hard_halt` predicate, handled by its own arm, untouched by this fix.

**All four of the original rejection's charges, accounted for — three answered, one upheld.** The
`os.exit` "0.006s → 18.9s" measurement is a misattribution (see the safe-direction observation below);
dead-stdout promptness is kept (the hard-halt cancel flag above — and still kept after W7-5d retired
that flag for this case, now via `stream_halt`'s per-job fault); the `reduce_task_slots` line-set
divergence is fixed by W7-5c below. The fourth — **"lets a faulting job leave a runaway sibling
unkillable"** — is **upheld and accepted by design, not fixed**. Run-all means a sibling that never
reaches a cancellation point (a tight loop with no I/O, a blocking sleep) now blocks `shutdown()` even
after another job has already faulted, on both engines. Reproduced on this HEAD:
```
ex.submit(boom); ex.submit(fn(): while true: n = n + 1); recover: ex.shutdown()
→ rc=124 (hangs) on BOTH engines
```
Pre-fix, the sibling's fault tripped the drain's own cancel flag and the spinner died at its next
back-edge — exactly the fast-fail behavior run-all deliberately removes for ordinary faults. The
correct remedy is caller-driven early-stop via `std.cancel.Token` (see the decision above), not a
return of the drain's own abort. Say this plainly rather than let three answered charges read as if
all four were answered.

**Measured pre-fix baseline (`6691b565`).** M:N's drain already ran every queued job's side effects —
a fault-first test with three good sibling jobs summing `1 + 10 + 100` already read back `111` — but
`submit_result`'s result-channel wrapper lost the result of a job it had just finished: a worker could
complete `f()` (side effects landed) and then observe a sibling's cancel at the next back-edge, BEFORE
the wrapper's `ch.send` ran, discarding it. So the pre-fix M:N shape was doing the queued jobs' work and
then still handing back 0 results for at least one of them — the drain's cancel flag was undoing work
it had already let happen, rather than preventing it. Serial's failure was the more visible half:
`--serial` genuinely aborted remaining siblings on the first fault, running 0 of the 3 good jobs where
M:N ran all 3.

**Landed:** `0127cfd7` (`src/vm/mod.rs` `executor_hard_halt` + `run_outcome` gating,
`src/vm/netio.rs` serial drain loop, `src/vm/sched.rs` doc comment), `af3fb10b` (review fixes: hard
halt now outranks an earlier ordinary fault when selecting which error `reduce_task_slots` propagates,
via a second `first_hard_fault` accumulator alongside the existing `first_fault`). Acceptance:
`tests/chz/stdlib/executor_drain_test.chz`, gated serial==M:N by
`test_runner::tests::chz_suite_passes_both_engines`.

### W7-5c — `reduce_task_slots` flushed a faulting task's buffered output only for the lowest-index fault (**FIXED 2026-08-01**)

Under the old abort-on-first-fault semantics this was latent: the drain's cancel flag made every other
task `Cancelled`, and the `Cancelled` arm always flushed, so a second task's output never had a chance
to go missing via the gated `Fault` arm. W7-5's run-all decision made it live — two jobs can now
genuinely both reach `TaskOutcome::Fault` in one drain, and the second one's buffered stdout/stderr was
being silently dropped (`sched.rs`, `reduce_task_slots`, gated `if first_fault.is_none()`). Fixed by
moving the flush out of that gate so every faulting task's buffered output flushes at its task-order
slot, unconditionally — matching the `Cancelled`/`Deadlocked` arms' shape. Which error PROPAGATES is
unchanged: still strictly the lowest-index fault (subject to W7-5's hard-halt-over-ordinary
precedence). `reduce_task_slots` is shared by 7 call sites including the `parallel:` nursery paths, so
this also closes the same latent gap on the nursery paths — plausible but unpinned: a nursery trips its
cancel flag on the first fault, so a second slot landing `Fault` (rather than the already-flushing
`Cancelled`) needs two tasks to fault before the cancel can take effect, which is inherently racy to
force deterministically, and no such test exists today. Landed: `05204777` (the fix), `0611f8ae`
(docblock-accuracy review fixes,
comment-only). Acceptance: `executor_second_faulting_job_keeps_its_output_both_engines`
(`src/vm/tests.rs`).

### W7-5b — an `Executor` created INSIDE a task was silently discarded — **FIXED 2026-08-03 (eager-execution milestone)**

Found while prosecuting the W7-5 fix: an M:N task registered a nested `Executor` in its own throwaway
worker `Vm.executors`, which `run_outcome`/`into_fiber` drop when the task finishes — the nested
executor's jobs never ran, were never reaped, and no fault was raised. `drain_live_executors` only ever
snapshotted the PARENT `Vm`'s list, so it never saw the child's. Reproduced on the pre-change binary
(`5960052`): a `spawn:` that builds an `Executor` and submits two printing jobs prints neither line on
M:N (`main done` alone) while `--serial` prints both.

**Why the first two attempts stalled, and what actually fixed it.** Both tried to make the per-`Vm`
handle list work: drain a fiber's own executors at `Disp::Finish` (task end — the scope-bound lifetime
decision D1 rejected), which then needed `swap_ctx`'s `executors` field moved out from under its
`ctx.heap`-only gate to make serial agree, dragging in GC rooting for a parked parent ctx that does not
exist. That was the explicit STOP condition Task 3 halted at, and the first eager-execution attempt
re-broke D1 and D3 together by reaping at task end on both engines.

The fix does not touch that gate at all. It changes **what the exit join walks**: a heap-independent
`ExecRegistry` (`Arc<Mutex<Vec<Arc<ExecutorCore>>>>`, `src/vm/core.rs`) that `spawn_worker` SHARES with
every worker. A core lives outside every heap by construction (B3.1), so an executor created anywhere
in the run is reachable by the one join regardless of which heap made it — no fiber-lifetime question,
no rooting question. `Vm.executors` is untouched and still drives the `--serial` reap, which drains
through the handle. The preserved `.superpowers/sdd/task-3-mn-half.patch` was **not** applied; it
implements the rejected lifetime and is now only of historical interest.

Acceptance: `executor_created_inside_a_task_is_joined_at_exit_both_engines` (`src/vm/tests.rs`),
compared as a line set on both engines under a watchdog.

**A sibling bug fell out of fixing this, and is also fixed.** The exit reap iterated a SNAPSHOT of the
executor list taken before it began, so an `Executor` created by a job that the reap was ITSELF running
was never in that snapshot and its work vanished — silently, on BOTH engines (verified on the
pre-change binary). Same symptom as W7-5b, different cause: W7-5b is heap visibility, this is iteration
order. Fixing either would have left the other, and fixing only the M:N side would have converted a
shared bug into a live serial-vs-M:N divergence. Both engines now re-scan until no un-shut executor
remains, terminating on the `shut` flag `shutdown` sets before it runs anything. Acceptance:
`executor_created_by_a_joined_job_is_also_joined_both_engines`.

### W7-12 — an eager `Executor` job blocked on a channel only its own joiner could fill HANGS on M:N — **FIXED 2026-08-03**

Found by the project owner while reviewing the eager-execution milestone (§2c).

**The program** (`ex.submit(fn(): ch.recv())` → `ex.shutdown()` → `ch.send(42)`): the job blocks on an
empty `recv`; `shutdown()` waits for the job; `main` can never reach its `send` because it is inside
that wait. A genuine deadlock, and genuinely the caller's mistake — the complaint is only about how it
is REPORTED.

| | pre-eager (`b6cb9201`) | after §2c | after the fix |
|---|---|---|---|
| M:N | faults in 0s | **hangs forever** | faults in 0s |
| `--serial` | faults in 0s | faults in 0s | faults in 0s |

Both legs measured on built binaries, not reasoned. So §2c traded a clear instant error for a silent
hang on the default engine AND opened a serial-vs-M:N divergence.

**Why it is not a simple revert.** §2c's change (an eager job BLOCKS on an empty `recv` instead of
faulting) is correct for the case it targeted — a job waiting on a value `main` sends on the next line,
which faults pre-§2c and works in Python/Go. The `netio.rs` arm decides by "am I inside a scheduler?",
which cannot distinguish the two programs, so both the old always-fault and the new always-block are
wrong half the time. Reverting re-breaks the producer/consumer shape the milestone existed to fix.

**The distinguishing question is "is this executor already being JOINED?"**
* job blocks while `main` is still running → a send may yet come → block (correct today).
* job blocks while `main` is inside `shutdown()` waiting for it, and no sibling job is runnable →
  nobody can send → fault.

**The interim fix, as shipped.** Two counters on the shared `ExecutorCore` (`src/vm/core.rs`) —
`joining` (threads inside an explicit `shutdown()` join, bumped by `JoinGuard`) and `blocked` (jobs of
this executor parked in an eager blocking loop, bumped by `BlockGuard`) — plus
`Vm::eager_core: Option<Arc<ExecutorCore>>`, which REPLACES the old `eager_job: bool` (the bool was
exactly `eager_core.is_some()`, so carrying both would only invite drift). `Vm::prepare_eager_job` sets
it, and `Vm::eager_join_deadlocked` (`src/vm/netio.rs`) asks
`joining > 0 && outstanding > 0 && blocked >= outstanding`. `Vm::eager_halt_check` checks it AFTER the
`--timeout` and cancel halts, so both still outrank it, and it therefore covers all three eager
blocking sites at once (`eager_wait_tick` serves the empty-`recv` and full-`send` loops; the `wait:` arm
calls it directly). Each site passes its OWN pre-existing message, now hoisted to consts beside
`FULL_SEND_DEADLOCK` — `EMPTY_RECV_DEADLOCK` and `EMPTY_WAIT_DEADLOCK` — so the restored fault is
byte-identical to `--serial`'s (verified by running the repro on both engines and diffing, not by
reading).

Four details that are load-bearing and were NOT in the original recipe (found by stress-testing it
against the code before implementing):

* **`joining` is bumped at the `shutdown` CALL SITE, not inside `Vm::join_eager_jobs`.** That function
  also serves `drain_live_executors`, which joins live executors one at a time in REGISTRY order, so
  bumping there would let the registry order decide which executor's job gets the fault.
  `shutdown_now` needs no bump either: it trips `cancel` first, and the cancel halt pre-empts this
  check.
* **…and only when the joiner has no live siblings** (`Vm::join_has_no_live_siblings`: no `MnSched`, no
  inline `parallel:` builder, no cooperative nursery, not itself an eager job, not in a native
  callback). Without this gate the fix RE-OPENED an engine divergence in a new place — found by
  adversarial review, not by the suite, and measured:
  `parallel: { spawn: timer(200); ch.send(42) } { spawn: ex.shutdown() }` printed `job got 42` on
  `--serial` and faulted on M:N. A `shutdown()` running inside a nursery task says nothing about
  whether a value can still arrive, because a SIBLING task can be the producer. Regression test:
  `executor_job_keeps_waiting_when_shutdown_runs_beside_a_live_producer` (mutation-verified — it fails
  when the gate is stubbed out, and it needs a real `timer` producer, slower than the debounce, or it
  passes with the bug present).
* **The `wait:` arm arms its `BlockGuard` BEFORE the halt check**, or the observing job does not count
  itself (`blocked` 0 vs `outstanding` 1) and a lone `wait:`-blocked job could never fault.
* **The full-`send` arm attempts `enqueue_bounded` ONCE outside the guard.** `submit_result`
  (`std/concurrency.chz`) ends every job with a cap-1 send that always has space; arming first would
  make `blocked == outstanding` transiently on every one of those jobs.
* **A two-observation debounce** (`Vm::eager_block_suspect`): the verdict fires only on two CONSECUTIVE
  positive observations, because `main` doing `ch.send(7)` then `ex.shutdown()` can otherwise be seen as
  "joining, and I am blocked" by a job whose `pop` failed a microsecond BEFORE the send landed. The
  failed re-`pop` / re-`enqueue` / `wait:` re-poll between the two observations is what proves no value
  arrived. Cleared when a block completes successfully — NOT on entry, since the `wait:` arm has no loop
  to enter and would reset it every tick.

**The correctness bar this fix is held to, because it was got wrong once.** The first cut faulted on
`x.submit(consumer)` / `y.submit(producer)` / `x.shutdown()` — a program **both ancestors run to
completion**, and which §2c's M:N ran to completion too. That was defended in this file with "the same
program faults on `--serial`, so no engine disagrees", which is an argument about AGREEMENT and not
about CORRECTNESS: two engines can agree on a wrong answer, and `--serial` faults there only because of
its own queue-at-`submit` model (D3) — an engine that is scheduled for REMOVAL (`future.md` §2b) can
never be the standard of correct. Reporting a deadlock in a program that has none is a wrong answer
about a live program — the [[no-drift-from-popular-languages]] rule, and precisely the failure mode of
bending the language to keep the oracle tidy. Fixed by the registry sweep in
`Vm::eager_join_deadlocked` (silent while any OTHER executor still owes work); pinned by
`executor_job_keeps_waiting_while_another_executor_still_owes_work`.

**Measured against the ancestors** (Go 1.26 + CPython, run on 2026-08-03, not reasoned). Go is the
concurrency ancestor and therefore the baseline for the deadlock VERDICT; `Executor` itself is
Python/Java lineage:

| program | Go | CPython | Chezzi (after the fix) |
|---|---|---|---|
| this gap's repro (job blocked, only its joiner could send) | `fatal error: all goroutines are asleep - deadlock!` | **hangs forever** | **faults** — Go-correct, and strictly better than Python |
| producer in ANOTHER executor | `got 1 / end` | `got 1 / end` | `got 1 / end` |
| producer is a sibling task, the `shutdown` itself in a task | `job got 42 / end` | — | `job got 42 / end` |
| two groups deadlocking EACH OTHER | `fatal error: … deadlock!` | hangs | **hangs** — Go is stricter; residual (a) |

Two things follow. The fault this gap restores is not merely "what pre-§2c did" — it is what Go does,
so the verdict is right on its own merits. And residual (a) is a MEASURED gap against Go rather than a
self-declared trade: Go reports that deadlock and we do not. Note also that Go's detector is exactly
the process-wide "all goroutines asleep" quiescence check `future.md` **§2d** proposes — the ancestor
already validates that roadmap, which is another reason not to keep widening this local predicate.

**The standing bar throughout: when a verdict is unsure, it must HANG, never fault.** An accepted hang
on a real deadlock is a missing answer; a fault on a working program is a wrong one. That ranking
survives the fix below unchanged — it is what the new detector's error-direction table encodes.

**The residuals below are CLOSED (2026-08-04) — see the `W7-12r` section further down.** They are kept
here as written, because the reasoning that made them necessary is exactly what the successor had to
answer, and because two of them were re-derived (and re-broken) while building it.

**Accepted residuals as they stood 2026-08-03** (ledger row `W7-12r`) — all of them the SAME decision,
"decline rather than answer wrong": (a) any executor holding MORE THAN ONE outstanding job hangs
instead of faulting, however plainly it is deadlocked, because `outstanding == 1` is the only shape
these counters can read without mistaking a healthy cap-1 handshake for a deadlock (see above); this
subsumes the earlier `--threads=1` and `wait:`-flicker residuals, which were both multi-job cases;
(b) two executors deadlocking each other likewise hang — the registry sweep silences the verdict while
any other executor still owes work; (c) a program with no explicit `shutdown()` still hangs at the exit
drain, per the first bullet above.

**The cost of (a) and (b) at full strength, because "it hangs" undersold it.** Both were MEASURED gaps
against Go, which reports them (`all goroutines are asleep - deadlock!`): `ex.submit(w); ex.submit(w);
ex.shutdown()` with two jobs on an empty `recv` — the commonest accidental executor deadlock there is —
hung forever. That was a deliberate ranking (a missing answer beats a wrong one), not a claim of
completeness, and it was the strongest single argument for scheduling §2d.

**Rejected experiment, recorded so it is not retried blind.** The obvious repair for (a) is a PROGRESS
counter: tick an `ExecutorCore::progress` on every completed channel handoff and fault only when every
job is parked AND the counter is unchanged across the debounce window — "parked and nothing moved".
Implemented and measured on 2026-08-03: **it still faults a healthy cap-1 pipeline 6/40 runs.** Instrumenting
the verdict shows why — `outstanding=2 blocked=2` with the progress stamp genuinely unchanged, mid-run.
The eager block is a 5 ms POLL (`DEMOTE_POLL_BACKOFF`) and a producer parked on a full `send` did not
always OBSERVE the consumer's `pop` — W7-13, fixed 2026-08-04: the wake was always sent, but the
waiter re-locked without re-checking, so it slept through it. Either way a healthy mutual handoff
really could make zero progress for a whole window. (W7-13's fix does not rehabilitate the counter:
the `wait:` arm still polls blind, and the objection below is semantic, not about latency.) Widening the window is just a timing knob with the same failure mode further out. The
lesson generalises past this predicate: **on a polling runtime, "nothing happened recently" is not
evidence that nothing CAN happen.** Only a real wait-for graph (§2d) answers this.

**Test coverage this fix RETIRED, stated rather than quietly dropped.**
`test_runner::tests::timeout_reaches_a_job_blocked_on_a_channel_and_on_wait` used to prove that the
eager blocking paths read `--timeout` THEMSELVES (a blocked job never reaches `jump_checked`'s
back-edge, where every other path observes the deadline). Its fixture was W7-12's repro, so it now
faults instead of hanging. It was reworked — a spinning sibling keeps `blocked < outstanding`, which
the predicate declines to judge — and still pins the end-to-end guarantee, but **no longer isolates
that deadline read**: verified by mutation (stub the read out and the reworked test still passes),
because the spinner's own hard halt trips the executor cancel flag and the blocked job leaves through
the cancel arm of the same check. Isolating it again needs a ONE-worker pool so the sibling never
starts, and `chezzi test` has no `--threads` (only `chezzi run` reads it / `CHEZZI_THREADS`), while the
pool is a process-wide `OnceLock` that cannot be resized in-process. Closing this means giving the test
runner a worker-count knob — a real flag with real docs, not a test-only hack, so it is filed here
rather than smuggled into this fix. The deadline read stays in place meanwhile: it is cheap, and every
argument that it is now redundant runs through the cancel cascade, which is a different mechanism.

**The standing rule for this predicate.** It is LOCAL — the same species as the one that sank the first
eager attempt — so keep it narrow and do not grow it opportunistically; adversarial review already
caught one over-reach (the missing sibling gate) that the whole green gate had not. The sound successor
(a process-wide AND-OR wait-for graph, knot detection, partial-deadlock capable) is designed in
`docs/future.md` **§2d**; that is the real fix, and it is its own milestone.

**Repro (both engines, on a release build):**
```
import std.concurrency
ex := Executor()
ch := Channel[int](1)
ex.submit(fn(): print("job got {ch.recv()}"))
ex.shutdown()
ch.send(42)
```
`timeout 15 ./target/release/chezzi run <f>` → before the fix, hangs (rc 124); `--serial` → faults in
0s. Both now fault in 0s with identical text. Scope that claim exactly: it holds for the program AS
WRITTEN, with the explicit `ex.shutdown()`. Delete that line and the executor is joined by the exit
drain instead, which deliberately does not arm the predicate — M:N then still hangs while `--serial`
faults, i.e. residual (d) is a still-live engine divergence, not merely an untested corner. Acceptance:
`executor_job_blocked_during_shutdown_faults_both_engines` (`src/vm/tests.rs`, watchdogged — the
failure mode of getting it wrong is a hang), with its mirror
`executor_job_blocking_recv_waits_for_a_later_send` — the case that must still BLOCK — kept green, and
the whole `tests/chz/stdlib/executor_drain_test.chz` (frozen by decision D5) unchanged. The `wait:` and
full-`send` sites were verified the same way on the CLI (a job whose only `wait:` arm, or whose second
cap-1 `send`, can only be served by its own joiner now faults identically on both engines). The
boundary — a `shutdown()` running beside a live sibling producer, which must still WAIT — is pinned by
`executor_job_keeps_waiting_when_shutdown_runs_beside_a_live_producer`.

### W7-13 — an eager `Executor` job's blocking wait DROPPED wakeups, so a healthy handshake stalled a whole 5 ms poll tick — **FIXED 2026-08-04**

Not a correctness bug — a latency/robustness one, and the reason W7-12's progress-counter experiment
failed. An eager job's blocking `send`/`recv` waits on `ChannelCore::cv` with a `DEMOTE_POLL_BACKOFF`
(5 ms) timeout, so a lost wakeup costs latency rather than the run.

**The original diagnosis in this section was WRONG, and is corrected here.** It read
"the recv→sender direction is the gap" and proposed adding a `wake_senders` to the eager `pop` path.
That wake was never missing: `Vm::wake_senders` already fires on all **six** pop paths — the demote
`recv` (`netio.rs:1254`), `recv` (`:1266`), `try_recv` (`:1286`), the `wait:` recv arm (`:1991`),
`demote_wait_block` (`:2117`) and `for v in ch:` (`exec.rs:2163`) — and for an eager job it lands on
`core.cv` (`mn`/`mn_enlist_sched` are both `None` there, so it takes the `notify_all` branch). The
proposed fix would have been a no-op duplicate. Worth recording as a method note: the report named a
real symptom, and the first mechanism that explains a symptom is not therefore the one that causes it.

**The real cause was a lost wakeup — the notification was sent, but nobody was on the condvar yet.**
`Vm::eager_wait_tick` handed a freshly-taken `core.q` guard straight to `cv.wait_timeout` with **no
predicate**, so the window between the caller's failed attempt and the wait was unguarded:

```text
enqueue_bounded(...)      # locks q, sees full, DROPS q, returns false
   <<< the consumer pops and calls core.cv.notify_all() HERE — nobody is on the cv: LOST >>>
eager_wait_tick:
   eager_halt_check(...)  # takes exec_registry + per-core `eager` — a WIDE window
   q.lock()
   cv.wait_timeout(q, 5ms)   # the notification is already gone -> sleeps the full tick
```

`eager_block_recv` had the identical shape (pop under the lock, `drop(q)`, halt check, then wait), so
the receive side carried the same latent bug; both are fixed by the one change.

**The fix.** `eager_wait_tick` takes a `ready` predicate and uses `Condvar::wait_timeout_while`, which
evaluates it under the guard *before* sleeping — so a wakeup that arrived while the lock was free is
observed instead of missed, and spurious wakeups are re-checked for free. Predicates are the callers'
own settle conditions: `g.len() < cap` for the full `send`, and `!g.is_empty() || g.closed ||
done_latch` for the empty `recv` (`done_latch` included because `trip()` also only does
`cv.notify_all()`, so it lost the wakeup the same way). `eager_halt_check` stays BEFORE the lock — the
no-lock-cycle argument on `eager_join_deadlocked` depends on `exec_registry` never being taken under
`ChannelCore::q`.

**Measured before → after** (first two rows: `chezzi run` on the release binary; third row: the
in-process test, debug profile — the profiles are not comparable to each other):

| program | before | after |
|---|---|---|
| release: cap-1 pipeline, 50 handoffs, 15 runs | `3,4` ms baseline but `8,9,14` ms in **7 of 15** — exact 5 ms quanta | **all 15 at 3–4 ms**, no outlier |
| release: cap-1 pipeline, 2000 handoffs, 10 runs | 10–33 ms | 9–11 ms |
| debug, in-process: 30 × 200-handoff pipelines | **2.19 s** | **0.14 s** |

**Why the regression test times 30 pipelines in aggregate rather than bounding one run.** A per-run
bound does not discriminate: only ~3 waits per 2000 handoffs actually lost their wakeup, so the
2000-handoff program ran in 10–33 ms *both* before and after, and the 50-handoff one stalls in only
about half of runs — a coin flip. Summing 30 × 200 handoffs gives the rare stall enough chances to
dominate, which is the 15× separation in the third row above;
`eager_handshake_is_driven_by_wakeups_not_by_the_poll_timeout` bounds that sum at 1 s (7× headroom
over fixed, 2× under broken), mutation-verified by reverting to the bare `wait_timeout`.

> **That bound does NOT survive a loaded full-suite run, and it is the last known one that doesn't
> (observed 2026-08-06/07, pre-existing — reproduced on a clean stashed tree at 5.34 s and again at
> 5.24 s with the `W7-26r` work applied).** `cargo test --lib` on a 12-core box at
> `RUST_TEST_THREADS=4` runs this beside the concurrency suites, and the 30-pipeline sum lands at
> **~5.2 s against a 1 s bound** — above even the 2.19 s "broken" figure, so under load the test
> cannot discriminate fixed from broken in EITHER direction. It passes alone in 0.2 s. Raising the
> bound is not the fix (past 2.19 s it stops detecting the bug it exists for); it wants a
> load-independent signal. **FIXED 2026-08-07 — and the answer was neither of the two options guessed
> here** (a serial-only `#[ignore]`d timing test, or a per-pipeline wakeup count). The test now asserts
> on the DEFECT ITSELF rather than on any duration: `BLOCK_WAITS_SLEPT_WHILE_READY` counts waits that
> burned a whole `DEMOTE_POLL_BACKOFF` tick and then woke to an ALREADY-READY channel — the lost
> wakeup, in one number. `wait_timeout_while` re-evaluates the predicate under the guard after each
> inner wait, so `timed_out()` implies "still not ready" and the counter is **structurally zero** in a
> fixed build, on an idle machine and a hammered one alike.
>
> **It is process-global like the rejected version below, and immune to that failure for a reason
> worth stating: it counts only an event a healthy build CANNOT produce.** A neighbour's honest 5 ms
> `timer(200)` park is a wait that expired while genuinely not ready, and never touches it; a
> neighbour could pollute this only by hitting the same defect, at which point failing is correct.
> (Total waits, `BLOCK_WAITS`, IS neighbour-polluted, so it is used only as a `>=` coverage floor —
> proof the pipeline still reaches `block_wait_tick`, so a refactor that stopped blocking there cannot
> leave the test passing vacuously.) Mutation-verified by reverting the call to the bare
> `wait_timeout`: **0 of 323 waits fixed → 309 of 1014 broken** (independently re-run by the
> controller: **270 of 849**). The bug is dense enough that 6 runs replace the old 30, and the test
> costs **0.05 s** instead of 0.21 s idle / 5.07 s loaded. Full `cargo test`: **4014 passed, 0 failed**,
> the first fully clean run in this series.
>
> **One disclosed false-positive path, found by review rather than reasoned away:** on a POISONED
> `core.q`, `wait_timeout_while` propagates the inner wait's `Err` without running its post-wait
> re-check, so the `into_inner` can report `timed_out()` on a ready channel. It needs another lib test
> to panic while holding a `ChannelCore::q` — a run that is already failing, since the bare `unwrap()`s
> elsewhere in `netio.rs` panic on that same poison — so the cost is a misleading second failure, never
> a false green. Recorded on the static's own doc comment.

**A rejected first version of that test is worth recording, because it produced a false green.** It
counted expired waits in a process-global `#[cfg(test)]` counter. libtest runs the file in ONE
process, and the eager tests a dozen slots away in name order each park a job on a `timer(200)` —
~40 expired ticks apiece, all landing on the same global. It passed alone and on a kindly-scheduled
full suite, then failed at 24 under `--test-threads=8` beside two of its own neighbours. Wall clock is
immune to that: a neighbour can steal CPU but cannot add to another test's elapsed time. Same family
as `lossy-decode-blinds-a-comparison-oracle` — when you add a detector, ask what else can move it.

**Three residuals were filed as `W7-13r`; ALL THREE ARE NOW FIXED (2026-08-04).** None was a
regression of this fix. Kept here because each says something the next reader needs.

1. ~~The eager `wait:` arm is a blind poll.~~ **FIXED — see W7-13r(a) below.** The first draft of this
   section claimed fixing it "needs a shared multi-channel wait primitive, a design change of its
   own"; **that was wrong, and adversarial review caught it** — `demote_wait_block`
   (`sched.rs:1114-1128`) already solved the same N-arm problem in four lines.
2. ~~`trip()` writes `done_latch` outside `core.q`.~~ **FIXED — see W7-13r(b) below.**
3. ~~The eager full-`send` loop never observes `closed`.~~ **FIXED — see W7-13r(c) below.**

Why it mattered beyond speed: it is what made "no progress in the last N ms" useless as evidence of
deadlock (see W7-12's rejected experiment), which is why it was fixed BEFORE the process-wide
quiescence detector (`future.md` §2d). Note that it does **not** make progress-rate reasoning sound —
the eager `wait:` block now wakes on arm 0 but every OTHER arm is still only observed once per
tick, and `parked-is-not-stuck` is a semantic objection, not a
latency one.

### W7-13r(a) — the eager `wait:` block was a blind sleep, so every wake cost a full 5 ms tick — **FIXED 2026-08-04**

`op_wait_poll`'s eager branch was `std::thread::sleep(DEMOTE_POLL_BACKOFF)` — no condvar at all, so a
`wait:` paid a whole tick however fast its value arrived. Fixed by waiting on **arm 0's** condvar with
the tick as the timeout: arm 0 wakes promptly, every other arm is still observed within a tick. Arm
0's readiness is evaluated under the guard the wait consumes (W7-13's rule).

**The predicate must mirror what the poll SETTLES on, arm kind by arm kind, and the first draft got
this wrong in the worst available way — a live-lock.** It read a recv arm as ready on `|| g.closed`,
but the poll *skips* a closed+empty recv arm. So the wait returned instantly, `ip -= 1` re-polled, the
arm was skipped, and the loop ran at CPU speed on Go's most ordinary `select`:

```text
wait:
    d := done.recv(): ...     # `done` is closed — a broadcast cancel
    v := work.recv(): ...     # the live arm
```

| | user CPU, 3 s wait |
|---|---|
| before the fix (blind sleep) | 0.01 s, **0%** |
| first draft of the fix | 3.00 s, **99%** |
| shipped | 0.01 s, **0%** |

Every variant printed the *right answer*, so no verdict-based test could see it. `MnSched::park_wait`
already documented the rule — "a closed+EMPTY non-timer recv arm is DEAD… op_wait_poll SKIPS a dead
arm, so requeueing on ONE dead arm among live ones spins… (the reverted parity-perf-0 live-lock)" —
and it was broken anyway. So the shipped predicate is derived from the poll's own arms:

* **RECV** ready == a queued value, or a `trip()` latch. **Not** `closed`, and a timer deadline is
  left to the timeout.
* **SEND** ready == space to enqueue, or `closed` — a closed send arm *is* acted on (the poll faults
  `CLOSED_SEND`, Go's panic-on-send-to-closed).

An earlier draft also claimed the wait was "strictly better than the sleep, never worse". It is better
for every arm-0 wake and no slower otherwise, but that phrasing is what stops a reviewer checking the
closed case — and the closed case was the live-lock.

**No timer clamp**, deliberately: a clamp to the soonest deadline was written here at first and was
**dead code**. `soonest` is provably `None` at this point (the cooperative inline-sleep above returns
for every `soonest.is_some()` case, and an eager job never takes the `mn.is_some()` branch); measured
identical timer behaviour with and without it, 304 ms vs 305 ms.

This is `demote_wait_block`'s existing trick (`sched.rs:1114-1128`), which is the point: the residual
was originally deferred as "needs a shared multi-channel wait primitive, a design change of its own",
and that was simply wrong — the precedent was already in the tree, four lines long. Adversarial
review caught the false claim, not the suite.

300 blocking `wait:` wakeups, release binary, same answer (`44850`) both ways:

| | before | after |
|---|---|---|
| wall clock | 1020 / 733 / 1102 ms | **5 / 5 / 5 ms** |

Fenced by `an_eager_wait_block_is_woken_by_its_arm_not_by_the_poll_timeout` (mutation-verified
in-process: 0.01 s green, 1.55 s red). **The `gate` handshake in that test is load-bearing** — the
first version let the producer race ahead, so every `wait:` found its value already queued, the block
branch was never reached, and the test passed even with the blind sleep stubbed back in. Note what
the gate does and does not give: `gate` is a buffered `Channel[bool](1)`, so the send returns without
a rendezvous and the guarantee is **statistical, not structural** — it bounds the producer to one
iteration ahead, which is enough to make the block branch overwhelmingly likely, not certain. That is
why the fence is a 300-iteration aggregate rather than a single run.

`an_eager_wait_with_a_closed_arm_still_takes_the_live_arm` pins the live-lock shape's semantics. It
CANNOT catch the spin itself: `cargo test` asserts verdicts, not CPU, and every variant of that bug
printed the right answer. The executable guard is the derivation comment at the predicate.

### W7-11 — an `RwShared` copy-out view of an element whose cycle closes through the ROOT container ABORTED THE HOST — **FIXED 2026-08-04**

The only ledger item that killed the process from a legal, single-threaded, checker-clean program.
Pre-existing on `main` (not a W7-4 regression, verified on `5960052`).

```chezzi
import std.concurrency
struct N:
    val: int
    back: List[N]
a := N(1, [])
xs := [a]
a.back = xs                  # the cycle closes through the ROOT container
rw := RwShared(xs)
rw.get()[0].val              # 1   — works, and always did
rw.at(0)                     # thread panicked: a wire Backref always targets an
                             # already-reconstructed node id   → rc=101, BOTH engines
```

**Mechanism.** `RwShared(xs)` stores a flat wire, `List{id:0, items:[Struct{id:1, back: Backref(0)}]}`.
A copy-out view clones `items[0]` and rebuilds it with its own empty `id -> GcRef` map, where id 0 —
the container, which the view never copied — is undefined. `elem_split` cannot help: it re-emits
**cell** definitions per depth-1 subtree, and the missing node is a **container**, which stays on the
pop-on-DFS-exit `path` discipline. Nothing on the store side was wrong; `get()`/`read()` rebuild the
same wire whole and tie the knot correctly.

**Fix** — `from_wire_memo`'s `Backref` arm no longer `.expect`s, and every piecewise drain goes
through one new entry point, `Vm::from_wire_piece(root, piece, rb)`:

1. **`WireValue::backrefs_resolvable(known)` is a PRE-check** — walk the piece and answer "can this be
   rebuilt alone, given what the map already holds?" *before* allocating anything;
2. resolvable → ordinary rebuild (the fast path, byte-identical to before);
3. not resolvable → rebuild the WHOLE `root` **into the caller's map** and return the piece by its wire
   id (`WireValue::node_id()`), so the node the back-reference wanted exists and the cycle is tied;
4. all 12 piece-draining sites in `rwshared_method` route through it, holding **the caller's** guard
   across the rebuild;
5. `Vm::wire_backref_missing` survives only as a backstop, asserted in `from_wire` and after the whole
   rebuild — it is no longer the control-flow signal.

**Points 1 and 3 are not cosmetic — they are the adversarial review's two findings, and the first cut
shipped both bugs.** Recorded because each was WORSE than the crash it replaced (silent wrong answers,
both engines agreeing, so parity-blind):

* **Attempt-then-react poisoned a shared rebuild map.** The first cut rebuilt the piece, noticed the
  flag, and *discarded* the result. But a half-finished attempt has already written partial nodes into
  the caller's map — including an `Obj::Cell` still holding the inert placeholder. `slice` shares ONE
  map across its elements by design (W7-4, so sibling closures land on one cell), so the next element
  hit the `Cell` first-wins dedupe and got the poisoned cell. Measured: `sl[0]() == 2` then
  `sl[1]() -> runtime error: type nil has no method 'len'`. Hence the pre-check: **nothing may be
  allocated until the piece is known to be rebuildable.**
* **A private map for the whole rebuild broke `slice`'s own contract.** `slice` is one call returning a
  container and is documented to share within itself; resolving the fallback into a private map made
  each cyclic element a separate copy. Measured: `sl[1].val = 99` then `sl[0].back[1].val == 2` where
  `get()` gives `99`. Rebuilding into the caller's map fixes it and makes every later piece resolve
  out of the same container.
* A third: **`node_id() == None` is not "cannot dangle".** A `Generator` carries no wire id (its parked
  frame can never be a `Backref` *target*) but reaches one through its backing closure, so the first
  cut returned the degraded placeholder for it. The pre-check is keyed on backrefs, not on ids, so it
  catches the generator; the fallback then rebuilds it against the completed map (a fresh node, which
  is right — it has no identity to preserve).

**Round 2 of the review found one more, in the fix for the second bug above.** "Rebuild into the
caller's map" is right, but *when* it happens decides the answer: `from_wire_memo`'s container arms
have **no** first-wins dedupe (only `Cell` does), so a whole-container rebuild triggered at element k
re-allocs and OVERWRITES `rebuild[id]` for elements `0..k` — which `slice` has already materialized and
pushed into its result. Identity therefore depended on element **ORDER**:

```text
only element 1 cyclic:   sl[0].val = 55  ->  sl[1].back[0].val
   CPython deepcopy:     55   (`sl[1].back[0] is sl[0]` -> True)
   Chezzi, second cut:   1    (orphaned copy — and `get()` on the same box says 55)
   Chezzi, shipped:      55
```

`slice` now makes the whole-container decision **once, before the first element** (`netio.rs`, one
`backrefs_resolvable` sweep over the selected indices), so every element is served from one container
whichever of them needs it. `at`/`for_each`/`fold`/… are unaffected — each is its own crossing with a
fresh map, which is their documented contract. Fenced by
`cyclic_slice_shares_when_only_a_LATER_element_is_cyclic`, mutation-verified; note that the earlier
`cyclic_slice_shares_within_itself_like_get` makes BOTH elements cyclic, so the fallback fires on
element 0 and it cannot catch this — the test that "already covers it" often doesn't.

Round 2 also corrected two claims this write-up made and one dead line: `at` is **not** the safe half of
a safe/dangerous pair on `RwShared` (there is no `RwShared[i]` — it does not satisfy `Index`; `at` is
simply its only read accessor, and it reports absence instead of faulting); the O(n) cost is CPython's
for a SINGLE piece but **not** for a whole-container walk (`for_each`/`fold` over a container where many
elements cycle is O(n²) — measured 0.068 / 0.28 / 1.17 s at n = 500 / 1000 / 2000 — where CPython's
`for x in deepcopy(xs)` is O(n); stated now on `from_wire_piece`); and a `push`/`pop` added around
`get_key`'s equality probe was dead, since `values_equal_guarded` takes `&self` and cannot collect.

**Why "rebuild the whole container", when W7-4 round 2 (`:4058`) rejected exactly that phrase.** It is
not the same fallback, and the two objections both dissolve:

| | W7-4 round 2 (rejected) | this |
|---|---|---|
| fires on | **every** piece (cell backrefs = ordinary data) | only a piece with a **dangling** backref = cyclic data |
| measured | 4000-elem `for_each` 0.011 s → 3.7 s; 12000 → 34 s | fast path unchanged; `rwshared_view_over_shared_bindings_is_not_quadratic` green untouched |
| torn read | re-read `core.v` under a **SECOND** guard → resolved a piece against an unrelated serialization (ids restart per serialization, so this was a wrong-NODE abort, M:N-only ⇒ parity-blind) | borrows the caller's live guard; the signature (`root: &WireValue`) makes a second acquisition impossible to write |

Holding one read guard across the rebuild is safe and is the window `at`/`slice` already held:
`from_wire*` allocs and nothing else, and `Heap::alloc` never collects, so no GC can run underneath and
re-lock `core.v` to mark `Obj::RwShared`. The guard is still dropped before any user code.

**The answer is CPython's, measured — not reasoned.** Chezzi must *copy* here (isolated heaps for the
M:N airlock), which puts it in CPython's position; Go and Rust never face the question because a shared
container hands out a pointer/`Rc`, not a copy.

```text
CPython:  b = copy.deepcopy(xs[0]); b.val = 42
          b.next[0].val                 -> 42     (b.next[0] IS b)
          b.next[0].next[0].next[0].val -> 42
          copy.deepcopy(one of 5)       -> copied list len 5   (the container came along)
Chezzi:   identity 42 42                           byte-identical, both engines
```

`pickle` agrees across a process boundary, so this is not a `deepcopy` quirk. The residual cost —
O(container) per view call **on cyclic data only** — is CPython's cost too, and is recorded as the
`ponytail:` ceiling on `from_wire_piece` (upgrade path: memoize the whole rebuild per (core, store
generation) across one walk).

**Shipped with it: `at(i) -> Option[E]`** (unrelated to the crash, requested in the same session).
`RwShared.at` was the only `at` in the language that faulted, against `std.json.at -> Option[Json]` and
its own sibling `get_key -> Option[V]`. Now `[]` is the dangerous index and `at` is the safe one; out
of range is `None`, negative indexing still normalizes, and a wrong container HEAD is still a fault
(that is a type error, not a missing element). 11 call sites, 4 files. This is NOT the
`min`/`max` → `Option` row above (23 call sites, still its own milestone).

**What did NOT change, and is worth knowing:** `contains`/`has` on a cyclic element/key still fault —
but now with the *catchable* `maximum structural depth (10000) exceeded`, from structural `==` on
cyclic data (the pre-existing documented limit, `cyclic_equality_errors_not_crashes`), reached only
because the rebuild in front of it no longer aborts.

**Fences.** `tests/chz/suites/rwshared_readview_test.chz` — 9 new `test fn`s covering every
piece-draining view (list `at`/`slice`/`for_each`/`fold`, map `get_key`/`for_each_entry`/
`fold_entries`, set `for_each`/`fold`, and `contains`'s recoverable fault) plus one per review finding
(`cyclic_slice_shared_map_is_not_poisoned_by_a_piece_that_needs_the_container`,
`cyclic_slice_shares_within_itself_like_get`,
`cyclic_generator_element_rebuilds_against_the_whole_container`,
`cyclic_slice_shares_when_only_a_LATER_element_is_cyclic`), gated serial==M:N by
`chz_suite_passes_both_engines`; identity is asserted by MUTATING the copy and reading it back through
its own cycle, not by `==` (which trips the depth cap on cyclic data). All four review fences are
mutation-verified against the exact code they came from: reverting the pre-check fails 2, reverting the
caller's-map rebuild fails a third, reverting `slice`'s decide-once fails the fourth. Rust:
`rwshared_view_of_a_container_cycling_element_does_not_abort_the_host` (two-engine parity) and
`rwshared_cyclic_view_round_trips_under_gc_stress` — both in Rust precisely because **the failure mode
is a dead process, not a red assert**. Mutation-verified: forcing `from_wire_piece`'s early return
makes the parity test fail with `cannot index nil`.

**Two method notes, both worth more than the fix.**

1. **The bug survived because the residual was documented, not fenced.** The piecewise-drain contract
   was written up in four places — `WireMemo`'s own type doc named this exact shape — and tested
   nowhere: no test ran a **cyclic** value through a **copy-out view**. Every existing cyclic test
   crossed a whole value (`spawn` arg, `Channel.send`, `Shared(...)`, `RwShared.get`); every existing
   view test used acyclic data. A documented residual is not a fenced one.
2. **The first cut passed the entire green gate and was still wrong.** 3801 unit tests, the full
   two-engine chz suite, conformance, clippy, the perf lock, plus a hand-written CPython comparison —
   all green on code that returned `nil` to user data. Two independent adversarial-review prosecutors
   found it within minutes, each with a running repro, because they attacked the SHARED-map caller
   (`slice`) that the fix's own author had only read, never exercised. The green gate measures what the
   suite already knows to ask.

### W7-16 — **a blocking NATIVE was not a cancellation checkpoint once the wait had STARTED** — **FIXED 2026-08-05** (filed 2026-08-04 as an eager-`Executor`-only contract question; measuring it showed the nursery was equally broken and `--timeout` reached no timer wait anywhere)

```chezzi
fn napper():
    print("napper start")
    time.sleep_ms(3000)      # or:  t := time.timer(3000); _ := t.recv()
    print("napper woke")     # <-- runs, AFTER the cancel
ex := Executor()
ex.submit(napper)
time.sleep_ms(50)
ex.shutdown_now()
print("main done")
```

Release binary, M:N, pre-fix: `napper start` / **`napper woke`** / `main done`, **@3005 ms** (timer
form: 3005 ms). Post-fix: `napper start` / `main done` **@55 ms**, both forms.

**Cause.** The offload path (`call.rs:~285`: `sleep_ms` rides the timer thread, wakes at the deadline)
requires an `MnSched`. An eager job has `mn == None`, so the native ran INLINE —
`std::thread::sleep` (`native/time.rs:34`), which observes no halt. `timer(ms).recv()` had its own
copy of the same hole (`netio.rs:~1655`), reached before the block-in-place path could see it. Same
root shape as W7-14: an inline sleep is a hole in every halt the loop it skips would have checked.

## The filed premise was WRONG in two ways — measuring it is what found the real bug

**(1) There was no nursery-vs-executor split.** The row claimed "the same `sleep_ms` inside a nursery
is interrupted on BOTH engines — that is an asserted contract". It is not what the fence asserted.
`parity_blocking_native_is_a_cancellation_checkpoint_on_both_engines` passed only because its `boom()`
divides by zero as its FIRST act, while `napper` still has a `print` ahead of the sleep: the cancel was
always tripped BEFORE the blocking call, so the fence covered the **entry** checkpoint
(`call.rs:276`) and nothing else. Move the fault 50 ms later — the same delay `shutdown_now()` has in
the executor repro — and the nursery behaves identically to the executor:

| construct | cancel @0 ms (entry) | cancel @50 ms (mid-flight) |
|---|---|---|
| nursery M:N, `sleep_ms(3000)` | 4 ms, cancelled | **3005 ms, `napper woke` prints** |
| nursery serial, `sleep_ms(3000)` | cancelled | **3054 ms, prints** |
| nursery M:N, `timer(3000).recv()` | cancelled | 55 ms, cancelled *(already parked — this one worked)* |
| eager `Executor`, either form | — | 3005 ms, prints |

The fence has been renamed `parity_blocking_native_is_an_entry_cancellation_checkpoint_on_both_engines`
and tightened (3000 → 1500 ms); the mid-flight half is
`tests::a_sleeping_nursery_task_is_cancelled_mid_flight_by_a_sibling_fault`, M:N-only because serial
cannot preempt a sleeping fiber at all.

**(2) The `--timeout` hole was not executor-specific.** `chezzi test --timeout=200` against three tests
that each sleep 3 s: **all three PASS**, 3 s each — top-level, nursery and executor alike; only a busy
`while` loop bucketed TIMED-OUT. A guard documented as a *hard abort* that silently never fires is
worse than no guard. Fenced by `test_runner::timeout_aborts_a_sleeping_test_everywhere` (6 fixtures ×
both engines — renamed from `..._on_every_block_in_place_path` when **W7-17** closed the park half).

## The contract question, and how it resolved

It resolved **against** the pairing this row proposed. CPython's
`ThreadPoolExecutor.shutdown(cancel_futures=True)` (measured 3001 ms, no interrupt) and Go's
`time.Sleep` are the **thread-blocking** sleeps. Chezzi's `sleep_ms` is a **fiber** wait, and both
ancestors DO cancel that one:

| spelling | cancellable? | measured |
|---|---|---|
| CPython `time.sleep(3)` in a `ThreadPoolExecutor` job | no | 3001 ms, runs to completion |
| CPython `await asyncio.sleep(3)` under a `TaskGroup` | **yes** | cancelled @50 ms |
| Go `time.Sleep(3s)` in a goroutine | no | never woken |
| Go `select { <-time.After(3s); <-ctx.Done() }` | **yes** | cancelled @100 ms |

Chezzi has ONE spelling where they have two, so the question is which it is — and the decisive evidence
is internal, not ancestral: an eager job blocked on a plain **`ch.recv()` already died at
`shutdown_now()` in 56 ms**. Exempting sleep was an inconsistency with the executor's own behavior, not
fidelity to CPython. `docs/concurrency.md:1342` and the parity fence's own doc-comment had also both
been promising this behavior for months.

## Fix

One rule, three seams:

1. `Vm::block_until_deadline` (`netio.rs`, beside `block_recv`) — wait in `DEMOTE_POLL_BACKOFF` (5 ms)
   chunks, running `block_halt_check` (deadline → cancel → quiesce) between them. Not a condvar: a
   plain sleep has no channel to wait on, and a waker would need notifying from every cancel-trip site
   *and* still need a timeout for the wall clock. Replaces the inline sleep in `invoke_native`
   (`call.rs`, covering the eager job / top-level `main` / serial fiber / `mn == None` callback) and in
   `chan_recv_step`'s timer branch (`netio.rs`).
2. The M:N offload re-arms its timer in 5 ms chunks (`arm_timer_sleep`, `mod.rs`) and ends the sleep on
   a cancel or the `--timeout` deadline. **Not** a park that `cancel_drain` could reach: that needs a
   claim-once token against the timer firing plus consistent `parked_n`/`runnable`/`inflight`, and a
   parked fiber no channel can feed is the W7-12/W7-15 false-deadlock shape. Re-arming keeps ONE owner
   (the timer heap) and leaves the counters byte-identical — `running -= 1`/`inflight += 1` still
   happen once in `offload`, `complete_offload` still runs exactly once.
3. `run_one_fiber`'s `resume_native` `Err` arm (`sched.rs`) now classifies a cancel-ended sleep as
   `Cancelled`, not `Fault` — otherwise the cancelled sleeper trips its siblings and MASKS the real
   error that cancelled it. Guarded by `executor_hard_halt` so a `--timeout`/over-memory abort is never
   swallowed into silence.

4. …and that `Err` arm must **UNWIND**, not merely finish: it returns without re-entering `run_until`,
   so nothing else runs the task's `defer`s. Without the explicit `unwind_deferred(0, false)` (same
   shape as `run_until`'s `cancel_bypass` funnel, hard-halt markers re-stamped), a cancel delivered
   mid-sleep silently skipped every registered cleanup while the same cancel 50 ms earlier — the entry
   checkpoint, which faults *inside* the VM — ran them. **Caught by adversarial review, not by the
   fix's own tests**, which asserted only "did it stop promptly" — and a task that skips its cleanup
   stops just as promptly. Now fenced by the `cleanup ran` assertion in
   `a_sleeping_nursery_task_is_cancelled_mid_flight_by_a_sibling_fault`.

**The sleeper is deliberately NOT registered as a blocked party.** Its wait always ends, so it is never
unsatisfiable; registering it would be a false-deadlock generator. Unregistered means `blocked < live`,
so the verdict declines — the safe direction, and exactly what `inflight` does on the M:N side.

**Scope, stated precisely** (all measured; the first draft of this section over-claimed all three):
`--serial` has the same checkpoint but nothing can trip it mid-sleep (one thread), so it is entry-only
in practice and gains the `--timeout` half only. `--timeout` reaches a `sleep_ms` everywhere and a
`timer(ms).recv()` on the two block-in-place paths, but **not** one parked in a nursery with no
runnable sibling — pre-existing, filed as **W7-17** and **FIXED 2026-08-05** (which also closed the one
inline-sleep this fix missed: the serial cooperative `wait:` timer arm). `--timeout` therefore now
reaches every timer wait, and — since **W7-18** (fixed 2026-08-05) — every netpoller park too.
`--max-heap` reaches a sleeper only through the
CANCEL arm (a nursery/`Executor` sibling sharing its cancel scope, 365 ms); a sleeping top-level `main`
has no cancel flag and its own heap is not the one growing, so it sleeps out first (3005 ms).

`join_eager_jobs`'s untimed `eager_cv.wait` was NOT touched, though the original row blamed it too: the
worker Vm carries the same absolute deadline, so a sleeping job now faults itself within 5 ms and
`finish` notifies the joiner. A second mechanism there would guard a hazard that no longer exists.

**Accepted cost, measured at both ends.** 200 timer re-arms/s per *sleeping* fiber (a `timers` mutex +
a poller notify each) — and they land on the SINGLE `chezzi-netpoller` thread that also drives socket
readiness, so at scale this contends with net IO, not just idle CPU. **200 concurrent sleepers**
(~40k re-arms/s): 300 ms of sleep takes 301.7 ms —
1.7 ms of overhead, nothing. **20 000 concurrent sleepers**, 2 s each: wall is
unchanged (2.127 s vs 2.112 s pre-fix) but CPU goes **0.88 s → 2.90 s** (user 0.31 → 2.38) — a pure
CPU cost on the single timer thread, and that is the regime to watch.
`ponytail:`-marked at `arm_timer_sleep` with the upgrade path (a per-scope pending-sleep registry
`cancel_drain` fires directly, so a sleep costs one timer entry again). Cancel latency is ≤5 ms, the bound every other blocking path here already pays. Sleep
accuracy is unchanged (300 ms sleep measured 300.1 ms top-level / 300.5 ms nursery) because each
re-arm targets the ABSOLUTE deadline, so tick jitter cannot accumulate.

### W7-17 — **`--timeout` had no path to a PARKED fiber**: a timer wait inside a `parallel:` nursery with no runnable sibling ran to its own deadline and then fell through — **FIXED 2026-08-05** (filed 2026-08-05 by adversarial review of the W7-16 branch)

`--timeout` is documented as a **hard abort**. It was not one for a fiber parked on a timer:

```chezzi
# a_test.chz — `chezzi test --timeout=300 a_test.chz`
import std.time
fn nap():
    tm := time.timer(3000)
    _ := tm.recv()
test fn t():
    parallel:
        spawn nap()
    assert false, "SWALLOWED"       # ← this ran
```

| shape (`--timeout=300`, no runnable sibling) | before | after |
|---|---|---|
| nursery `timer(3000).recv()` | **3004 ms**, `FAIL … assertion failed: SWALLOWED` | **304 ms**, `TIMED-OUT t` |
| nursery `wait:` with a `timer(3000)` arm | **3004 ms**, `FAIL … SWALLOWED` | **304 ms**, `TIMED-OUT t` |
| serial cooperative `wait:` timer arm | **3004 ms**, `FAIL … SWALLOWED` | aborted |
| the same, **plus a spinning sibling** | 303 ms, `TIMED-OUT t` | 303 ms, unchanged |
| Go `go test -timeout 300ms` + `<-time.After(3s)` | `panic: test timed out after 300ms`; the following `t.Fatal` never runs | — |

Stable at `--threads=1/2/3/4/8`/default (12/12 runs, 303–304 ms, `TIMED-OUT` and no `SWALLOWED`).

**Root cause.** Both M:N timer-park sites scheduled a one-shot wake at the **timer's own** deadline and
parked the fiber. A parked fiber reaches no `jump_checked` back-edge and no `block_halt_check`, so the
run's wall-clock deadline had no path to it; the pending timer is accounted `inflight`, which correctly
vetoes the deadlock verdict, so nothing else fired either. Cancel was never affected — `cancel_drain`
walks `c.parked`, so a sibling's fault reached this fiber in 55 ms all along. It is specifically the
wall clock that could not.

**Fix — a wake and a checkpoint, which are one fix.** Both `timer::submit_at` sites (`chan_recv_step`'s
M:N timer branch and `op_wait_poll`'s timer arm) now fire at `min(their own deadline, self.deadline)`
and deliver `Bool(true)` **only if their own deadline really passed**; an early fire goes through
`deadline_gap_wake` instead. The deadline read is split out of `block_halt_check` as
`Vm::deadline_halt` and called at **two** places in each op, and both placements were forced by review
(see the lessons):

- at the **top**, above the cancellation checkpoint (the deadline outranks a cancel, and the early
  wake trips a cancel) but **suppressed inside a `defer`** by the same `deferring > 0` term
  `cancel_requested` uses;
- at the **park**, ungated — everything above it settles without blocking, and a defer that would
  park past the deadline is a hang, not cleanup.

With `--timeout` off, `min` is the timer's own deadline and the job is byte-identical to the one it
replaces: one wake, one `inflight` add/sub, **no re-arming** — W7-16's 200-re-arms/s cost is not paid
here.

**The early wake must leave STATE, not just a wake.** `timer::submit_at` runs *before* the fiber is
actually parked — `park_recv`/`wait_suspend` only mark it; `MnSched::park`/`park_wait` do the parking
later, behind the core lock. A job firing in that window finds an empty bucket, so a bare `close_wake`
is **lost**, and the fiber then parks with its one job already spent (`op_wait_poll`'s `timer_armed`
CAS forbids a second arm) — a hang past the very deadline that exists to prevent hangs. The park-gap
re-check reads exactly four things: a queued value, `closed`, `done_latch`, and the fiber's **scope
cancel**. The first three all mean "the timer fired", which is a lie here, so the cancel flag is the
one truthful state that closes the gap — hence `deadline_gap_wake` sets it before waking, and hence
the top-of-op deadline check is ordered *above* the cancel check so the verdict is the honest
`timed_out`, not `cancelled`.

A third site fell out of measuring the fix rather than the bug: the **cooperative (serial) `wait:`
timer arm** in `op_wait_poll` was still a bare `thread::sleep(deadline - now)` — the one inline-sleep
W7-16's four seams missed — and now uses `block_until_deadline`. **N10 is unchanged**: it still sleeps
to the deadline and takes the timer arm without yielding to a runnable sibling; it just observes the
halts on the way.

**Lessons.**

1. **The filed lesson was wrong, and wrong in a way worth keeping.** The row concluded that fixing this
   needed "a deadline-driven wake (a scheduler feature), not another checkpoint", because
   "chunk-re-arming the park's timer job only gets a wake, and the resumed fiber would re-park". Every
   clause is true; the conclusion does not follow. A wake and a checkpoint are the fix **together** —
   neither alone is anything, and the re-park it predicted is precisely what the missing checkpoint
   prevents. A "this needs machinery X" verdict deserves the same evidence bar as a bug report.
2. **The scheduler-level alternative the row pointed at is worse, not just bigger.** `flag_deadlock`
   drops parked fibers without `unwind_deferred`, so faulting them from the scheduler would have
   re-introduced W7-16's own skipped-`defer` bug. Wake-and-re-check faults from *inside* the VM, so the
   task unwinds normally and its cleanup runs — verified by a marker file written from a `defer` on the
   aborted path, and fenced by `a_timer_parked_task_aborted_by_the_deadline_still_runs_its_defers`.
3. **A runnable sibling in the neighbouring fixtures hid this for a whole milestone.** Every W7-16
   timeout fixture had one (or was a block-in-place path), and a spinner's own back-edge trips the
   deadline at 303 ms — indistinguishable, in the report, from the park being reached. The new fixtures
   have no sibling on purpose.
4. **The fix's FIRST cut re-introduced W7-16's own bug, and shipped fully green.** With
   `deadline_halt` at the top of both ops and ungated, a `defer`'s cleanup `ch.recv()` on an **already
   queued** value silently vanished — measured against a HEAD binary, the `DEFER-RECV 7` line simply
   missing. Every test passed, *including* the new `..._still_runs_its_defers` fence, because that
   fence's `defer` only calls `print`. The rule the cut broke was already written down two functions
   away (`cancel_suppressed` = `cancelled || deferring > 0`) and was not consulted. Two adversarial
   prosecutors found it independently; the suite found neither it nor the park-gap race above. Now
   fenced by `the_deadline_does_not_truncate_a_defer_whose_recv_can_complete` (mutation-verified red
   when the check is hoisted back above the pop).

Fenced by `test_runner::timeout_aborts_a_sleeping_test_everywhere` (renamed from
`..._on_every_block_in_place_path`, which was named that way *to* fence this by omission; 6 fixtures ×
both engines, 2 red pre-fix), `a_timer_parked_task_aborted_by_the_deadline_still_runs_its_defers`,
`the_deadline_does_not_truncate_a_defer_whose_recv_can_complete`, plus
`a_live_timer_still_delivers_under_a_generous_timeout` for the other direction of the clamp — a live
timer must still deliver at its own deadline, and a sibling value must still beat a timer arm (W7-14's
shape, M:N-only per **N10**). **The park gap is argued, not timing-tested**: it is a sub-millisecond
submit-to-park window with no deterministic trigger (300 probe runs did not hit it), so the reasoning
lives in `deadline_gap_wake`'s doc-comment instead of a test that would pass either way.

### W7-18 — **`--timeout` could not reach a NETPOLLER-parked fiber, and that one HUNG** — **FIXED 2026-08-05** (filed 2026-08-05 while measuring W7-17)

Same root as W7-17 (no path from the wall-clock deadline to a parked fiber), a strictly worse symptom —
W7-17 fell through *after* its timer expired; this never ended at all:

```chezzi
# c_test.chz — `chezzi test --timeout=300 c_test.chz`
import std.net
fn serve():
    match net.listen("127.0.0.1:0"):
        Ok(l): _ := l.accept()          # nothing ever connects
        Err(e): print("no listen")
test fn t():
    parallel:
        spawn serve()
    assert false, "SWALLOWED"
```

| shape (`--timeout=300`) | before | after |
|---|---|---|
| nursery `spawn` on an untimed `l.accept()` | **10001 ms**, no verdict, no output (external `timeout 10` kill) | **304 ms**, `TIMED-OUT t` |
| nursery `spawn` on `net.connect("192.0.2.1:9")` (TEST-NET-1) | same hang | **304 ms**, `TIMED-OUT t` |
| the aborted task's `defer` doing `conn.write("bye")` | never reached | `TIMED-OUT t` + `DEFER-WROTE 3` |
| **top-level** `net.connect("192.0.2.1:9")` in the test body | 10 s spin, then `FAIL … SWALLOWED` | **304 ms**, `TIMED-OUT t` |
| `accept(150)` under `--timeout=5000` (the other direction) | `Err("timeout")` at 154 ms | unchanged — still catchable |
| Go `go test -timeout 300ms` + goroutine on `net.Listener.Accept()` | `panic: test timed out after 300ms`; the following `t.Fatal` never runs | — |

Stable 10/10 at `CHEZZI_THREADS=1/2/3/4/8`, 303–304 ms, `TIMED-OUT` and no `SWALLOWED`.

**Root cause.** `PollPark.deadline` carried only the *socket op's own* `timeout_ms` (D6c). `None` = park
forever, and a `None` deadline is invisible to BOTH `next_timeout` (so the poll thread's own `wait` is
unbounded) and `fire_due_socket_timeouts`'s `deadline.is_some_and(|d| d <= now)` filter. A socket park is
also accounted `inflight` — correctly, the OS could still wake it — so the deadlock verdict rightly
declined too. Nothing in the process was watching the wall clock on behalf of that fiber.

**The fix — and the filed premise it refutes.** The row above concluded that a run-deadline abort "needs
a second marker distinct from `poll_timed_out`, threaded through the 5 `PollPark` construction sites plus
the re-inject", because the existing marker makes the rewound op return a *catchable* `Err("timeout")`
while a hard halt must not be catchable. **No second marker was needed.** `Vm::deadline` is already an
absolute `Instant`, already threaded onto every worker (`sched.rs`, `worker.set_deadline`), and `Some`
only under `chezzi test --timeout` — so asking `now >= self.deadline` **at resume, before consuming
`poll_timed_out`** answers precisely the question the marker would have carried. `PollPark`,
`poller::register`, `next_timeout` and `fire_due_socket_timeouts` are untouched; so are the two literal
`PollPark` constructions in tests, which the marker design would have forced to change.

Four seams, all in the VM:

- **`park_on_fd`** (the shared park for `read`/`read_bytes`/`write`/`accept`) clamps `target.deadline` to
  `min(op deadline, run deadline)` and gains a `deadline_halt` **above** its cancellation checkpoint
  (W7-17's ordering — the deadline outranks a cancel), **ungated** by `deferring`: a `defer` that would
  park past the deadline is a hang, not cleanup.
- **`poll_timeout_check`** is the new shared resume guard replacing the four duplicated
  `if self.poll_timed_out { … }` entry heads. It **takes the flag first**, then halts.
- **`park_on_connect`** sets `deadline: self.deadline` (a `connect` has no `timeout_ms`, so the marker on
  a connect resume is unambiguous), and the `pending_connect` arm of `run_one_fiber` **raises** the halt
  rather than clearing the flag.
- **`demote_block_socket`** (the in-callback / block-in-place path) checks the deadline per iteration,
  with `break Err(e)`, never `?`.

**No `deadline_gap_wake` analogue is needed**, and the asymmetry with W7-17 is the interesting part.
There, `timer::submit_at` armed a *job* before `MnSched::park` had filled the fiber's bucket, so an early
fire found an empty bucket and was LOST. Here the deadline is not a job but a **field in the registry
row**, and `poller::register` inserts the row and the fiber together under the registry lock; the wake is
re-derived by re-reading the registry. An already-expired row simply makes `next_timeout` return `ZERO`.

**Lessons.**

1. **"This needs machinery X" deserves the same evidence bar as a bug report** — this is W7-17's lesson 1
   recurring one row later, in the same file, about the same subsystem. Both rows named a mechanism the
   fix turned out not to need. The tell is identical both times: the row reasoned from what the existing
   machinery *carries* rather than from what the question actually *is*. "Which deadline expired?" is a
   question about the clock, and the clock was already there.
2. **The obvious spelling of the right idea re-introduced W7-16's skipped-`defer` bug on THREE paths.**
   Halting with `?` before consuming `poll_timed_out` leaves the flag set for the unwind, so the first
   socket op in any `defer` reports a fabricated `Err("timeout")` and the cleanup silently does nothing.
   A `?` in `demote_block_socket` returns past `demote_socket_exit`, permanently leaking
   `running`/`inflight`. Clearing the flag at the connect resume instead of raising lets the fiber finish
   normally, so the nursery joins and `assert false` reports `FAIL … SWALLOWED` — W7-17's original
   symptom, re-created by the fix meant to close it. All three are mutation-verified red by the new
   fences; all three would have shipped green without them.
3. **Adversarial review found a fourth, on a path the plan had classified as a mere overshoot.** A
   top-level `net.connect` (the test body has no `mn`, so it takes `block_until_connected`'s bounded
   spin) had the run deadline come back as a **catchable** `Err("connect failed: timed out")`:
   `FAIL … SWALLOWED` at 304 ms, a `--timeout` a `match` arm swallows. Clamping the spin was necessary
   and *not sufficient* — a function returning a `Value` cannot raise a halt, so the raise had to move to
   the call site. Two independent prosecutors filed it; the full green suite, the four new fences and the
   plan's own adversarial pre-review had all missed it.

Fenced by `test_runner::timeout_aborts_a_netpoller_parked_test` (accept + connect park, both
construction paths), `timeout_aborts_a_top_level_connect`,
`a_netpoller_aborted_task_still_runs_its_defers` (asserts `DEFER-WROTE 3` — the byte count, so a
cleanup that ran but wrote nothing still fails), and the counter-fence
`a_socket_timeout_is_still_catchable_under_a_generous_timeout`. All four run through
`run_tests_timed_watchdog`, a side thread + `recv_timeout(10s)`: a regression here HANGS rather than
fails, and inline that would wedge `cargo test` itself — which is why these could not join
`timeout_aborts_a_sleeping_test_everywhere`, whose doc-comment previously fenced W7-18 *by omission*.

**Not fenced, deliberately:** `register`'s cancel-gap ordering (a sub-millisecond window with no
deterministic trigger — same call as W7-17's park gap, argued in the doc-comment instead); the
`demote_block_socket` accounting leak (a `?` there is caught by review and the comment, not by a test);
and the degeneracy where an op deadline shorter than the run deadline by less than poller-inject-to-
schedule latency converts a catchable `Err("timeout")` into a hard halt (correct by consequence — that
fiber hard-halts at its next checkpoint regardless — and a test would pass either way).

### W7-14 — **WAIT-1, unfixed on every block-in-place path**: a `wait:` timer arm inside an `Executor` job (or on top-level `main`) inline-slept to the deadline and could not take a sibling value that arrived sooner — **FIXED 2026-08-04, found the same day while reviewing W7-13r(a)**

**This is the bug `WAIT-1` already fixed — on a path its gate does not reach.** `0b72ad60`
("M:N timer lost-wakeup — timed-park instead of inline-sleep", 2026-06-13) replaced the inline-sleep
with a background deadline `send_wake` + snapshot-park, and that branch is gated on
`self.mn.is_some()`. An `Executor` job has `mn == None`, so it falls straight past WAIT-1's fix into
the **cooperative inline-sleep** above the eager block — which sleeps to the deadline and takes the
timer arm without looking at the other arms again.

Same logic, two shapes, release binary:

| shape | result |
|---|---|
| `parallel:` / `spawn:` (WAIT-1's snapshot-park path) | **`value 9` at 54 ms** — correct, matches Go |
| the same `wait:` inside an `Executor` job | **`timer` at 305 ms** |

**Not a regression of eager execution**, though it looks like one: pre-eager `main` (`b6cb9201`, when
`submit` still queued and `shutdown` drained inline) measures the same 305 ms. The `Executor` path has
simply never had WAIT-1's fix, before or after §2c. W7-13r(a) only *surfaced* it — the dead timer
clamp is what exposed the control flow.

**Why WAIT-1's recipe does not port over unchanged:** it submits the background deadline send *into
`self.mn`*, and an eager job has no `MnSched` to submit to. That is the actual work here.

```text
cons:  t := time.timer(300)
       wait:
           _ := t.recv():      -> "timer"     # ALWAYS wins
           v := work.recv():   -> "value {v}" # even though it arrives at 50 ms
prod:  sleep 50ms; work.send(9)
```

Go's `select` takes the 50 ms value — this is the timeout arm beating the thing it is supposed to be a
timeout *for*, i.e. a `wait:` with any timer arm degenerates into a plain sleep inside an `Executor`
job. Identical before and after W7-13r(a) (305 ms vs 304 ms).

**THE FIX (2026-08-04).** Three edits in `op_wait_poll` (`src/vm/netio.rs`), no new machinery:

1. `timed_block = soonest.is_some() && self.owns_os_thread()` — the new
   `owns_os_thread()` is `is_counted_party()` **minus** its `native_reentry == 0` clause (that clause
   is about whether the deadlock verdict may JUDGE a party, not whether it may block);
2. the cooperative inline-sleep is gated OFF for `can_block_in_place() || timed_block`, so only a
   cooperative FIBER still takes it (it has no thread to clamp, and its `wait_suspend` park has no
   timer wake — removing the sleep there would hang a timer-only `wait:` outright; N10 is unchanged);
3. the block-in-place branch admits `timed_block` too, and **clamps its condvar timeout to the soonest
   deadline** — `DEMOTE_POLL_BACKOFF.min(deadline - now)`. The branch already re-polls (`ip -= 1`),
   and the poll's own `now >= deadline` arm then takes the timer; before the deadline it is an
   ordinary arm-0 wait, so a sibling's value wins.

Measured on the release binary, the repro above:

| waiter | before | after |
|---|---|---|
| inside an `Executor` job (M:N) | `timer` @ 306 ms | **`value 9` @ 56 ms** |
| top-level `main` (M:N) | `timer` @ 306 ms | **`value 9` @ 56 ms** |
| top-level `main` INSIDE a native callback (`[1].map(f)`) | `timer` @ 308 ms | **`value 9` @ 57 ms** |
| any of them on `--serial` | `timer` @ 305 / 355 ms | unchanged (nothing else can run) |

**Why `timed_block` is narrower than "block here whenever you own the thread".** An in-callback party
is not registered with the verdict (`is_counted_party` is false), so a block there can never be judged
a deadlock — for an UNTIMED wait that would turn today's honest `wait on channels that are all empty:
deadlock` fault into a silent hang. A live timer arm removes the risk entirely: the wait provably ends
at the deadline whatever anyone else does. `vm_wait_in_native_callback_no_sender_deadlocks` fences the
untimed half.

Fenced by `an_eager_wait_timer_arm_loses_to_a_sibling_value`,
`a_top_level_wait_timer_arm_loses_to_an_eager_job` and
`a_wait_timer_arm_in_a_native_callback_loses_to_a_sibling_value` (`src/vm/tests.rs`), each
mutation-verified — the callback one goes red on the `timed_block` widening ALONE, the other two on
the gate.

**The tests use `timer(3000)`, not the 300 ms of the repro, and that is a lesson of its own.** The
first cut asserted `elapsed < 250 ms` against a 306 ms bug — a 50 ms/306 ms window. Adversarial review
ran them inside a full concurrent `cargo test --lib` and they FAILED at ~2.2 s elapsed while passing
in isolation. The discriminator that actually matters is the OUTPUT (`value 9` vs `timer`), which is
load-independent only if the deadline is far beyond any plausible stall; a 3 s deadline gives that and
leaves a loose 1.5 s bound with 20× headroom on both sides. **A timing bound whose fixed and broken
values are within 6× of each other is a flake, not a fence.**

**A SECOND bug fell out with it, found while measuring the fix and previously unfiled: a timer arm
made an eager `wait:` UNCANCELLABLE**, and the job then ran the timer arm's body after the cancel.
`thread::sleep` observes nothing, and `op_wait_poll`'s cancellation checkpoint is at the TOP of the
op — so it could not fire until the sleep returned, by which time the deadline had passed and the
timer arm was taken. Measured on release binaries in separate target dirs (`shutdown_now()` at 50 ms
against a job waiting on `timer(3000)`):

| | before | after |
|---|---|---|
| output | **`timer` printed** | nothing printed |
| exit | **3007 ms** | **57 ms** |

The block-in-place path re-checks `block_halt_check` — cancel, `--timeout`, the deadlock verdict —
once per `DEMOTE_POLL_BACKOFF`, so all three now land within a tick of a timer-armed `wait:` instead
of within a timer deadline. Fenced by `a_timer_armed_eager_wait_is_cancellable_by_shutdown_now`.
Generalizable lesson: **an inline sleep is a hole in every halt the loop it skips would have
checked** — the latency was the visible symptom, the missed cancel was the real defect.

**Scope was widened deliberately, and the wider half is the more important one.** The filed row was
about `Executor` jobs, but `main` has `mn == None` too and took the identical branch — a top-level
`wait:` beside an eager job gave the same wrong answer, unfiled. Gating on `eager_core.is_some()`
would have closed the reported symptom and left the sibling live (CLAUDE.md's root-cause rule).

**Why the "does not port" blocker below was wrong.** WAIT-1 submits a background deadline `send_wake`
because a PARKED FIBER has no thread of its own to time out on. A block-in-place party *is* a thread:
it does not need a wake injected, only a shorter timeout. No `timer::submit_at`, no `inflight`
accounting, no `MnSched`. The second lesson is about the clamp W7-13r(a) deleted as dead code — it was
not dead, it was **unreachable because of this bug**, and recording "`soonest` is provably `None`
here" turned the bug into a documented invariant. A clamp/branch that is provably unreachable deserves
the question "why can't this fire?" before the deletion.

**Deliberately NOT changed: the serial cooperative fiber (N10).** It is the frozen parity oracle, its
fix is folded into the post-freeze serial removal, and it is the one waiter that genuinely cannot
clamp — it has no thread. `--serial` therefore still answers `timer` on the repro. Per CLAUDE.md that
is not a defense of anything: M:N matches Go and is the correct engine here.

### W7-13r(b) — `trip()` set `done_latch` outside `core.q`, so a waiter could still miss it — **FIXED 2026-08-04**

`close()` has always set `closed` under `core.q`, which is what lets a blocked waiter re-check it
under the guard the wait consumes. `trip()` used a bare relaxed atomic outside that lock, so a waiter
could evaluate "not tripped" while holding `q` and be notified before it had atomically released `q`
and enqueued on `cv` — the same lost-wakeup shape W7-13 fixed for values and closes, costing a full
tick. The store now happens under `core.q` (the guard dropped before the wake fan-out, so `q` is never
held across the sched lock). Holding `q` across the store leaves only two orderings: the waiter sees
the latch in its predicate, or it is already on the condvar when `notify_all` runs.

**Deliberately NOT fenced by a test, and this is the honest reason:** the window is the few
nanoseconds between the predicate evaluation and the condvar enqueue *inside* `wait_timeout_while`, so
it essentially never fires. Measured, 200 sequential trip-handshakes with a `gate` forcing the waiter
to arrive first: **5–6 ms before, 5–6 ms after** — no signal. A timing test here would assert nothing
and flake; the fix is correct by construction (it makes `trip()` obey the discipline `close()` already
follows), and that is the whole of its justification.

**The store alone was not sufficient, which review caught:** a waiter only benefits if it re-checks the
latch under `q`. `demote_wait_block` did not — it waited on the bare `q.is_empty()`, so a `trip()` cost
it a full tick regardless of where the store happened. Its predicate now includes `done_latch` too.
`closed` is deliberately absent from BOTH waiters' recv predicates, for W7-13r(a)'s live-lock reason.

### W7-13r(c) — an eager job blocked on a FULL channel never observed a `close()`, so it hung — **FIXED 2026-08-04**

Filed as a residual while fixing W7-13, then fixed the same day. **Pre-existing, not a W7-13
regression** — the old bare-`wait_timeout` loop had the identical hole.

`enqueue_bounded` never consults `closed`, and the eager block loop never returns to the
top-of-`send` closed guard, so a blocked eager sender had no way to observe a close *at all*. It was
rescued only by accident: the W7-12 deadlock verdict needs `joining > 0`, so a program with an
explicit `shutdown()` eventually got an answer — the **wrong** one. Remove the `shutdown()` and
nothing catches it.

```text
ch := Channel[int](1)        # cap 1
blocker:  ch.send(1)         # fills it
          ch.send(2)         # BLOCKS
closer:   sleep 100ms; ch.close()
```

| | before | after |
|---|---|---|
| M:N, no `shutdown()` | **HANGS** — killed at a 12 s timeout | faults at **105 ms** |
| M:N, with `shutdown()` | 112 ms, but reports `send on a full channel: deadlock — …no runnable task can receive…` about a channel that is CLOSED | 105 ms, `send on a closed channel` |
| **Go**, same program | `panic: send on closed channel` at **104 ms** | — |

(All release-binary wall clock, 3+ runs each. The Go figure is a **compiled binary**; an earlier draft
of this table quoted 182 ms, which was `go run`'s cold compile-and-link, not Go's runtime — the two
languages are the same speed here. An earlier draft also quoted 3114 ms for the `shutdown()` row; that
number came from a different program carrying an extra 3 s sleep, and the honest figure is 112 ms with
the *wrong answer*. The defect that shape shows is the misreport, not latency; the hang needs the
no-`shutdown()` row.)

The fix adds `g.closed` to the wait predicate (so `close()`'s existing `cv.notify_all()` wakes the
sender promptly) and checks `closed` **in the loop body**, faulting `CLOSED_SEND`.

Two things are load-bearing and easy to get wrong:

* **`closed` may not be acted on by the predicate alone.** A predicate that reports ready while
  `enqueue_bounded` keeps refusing turns a 5 ms poll into a **hot spin**. The body must fault.
* **The closed-check goes AFTER the enqueue retry, never before** — the reverse is a regression, and
  the first draft of this fix shipped it. On the ordinary drain-then-close shape
  (`a := ch.recv()` then `ch.close()`), the recv frees the slot *for* the blocked sender, and the
  closer then wins the race back to `core.q`; a closed-check placed first faulted a program **Go
  completes** (`sent both`, measured 5/5 each way). Go's receive hands the value to a waiting sender
  atomically inside the recv, so by the time `close` runs the send has already happened; Chezzi's
  eager sender is retry-based and must re-take the slot, so it has to retry FIRST. This is also the
  drain-before-close rule the top-of-`send` guard already documents. Fenced by
  `a_blocked_eager_send_still_completes_when_a_recv_frees_its_slot_before_the_close`.

**Precondition, not fixed here: this needs ≥2 free pool threads.** A blocked eager job holds its pool
thread with no replacement spin, so at `--threads=1` the closer is never dispatched and the program
hangs — measured identically before and after this fix, and on `main`. That is `pool.rs`'s recorded
"Known v1 hazard" (a fixed-size, non-growing pool), orthogonal to this gap and untouched by it.

`"send on a closed channel"` is now the const `CLOSED_SEND`, shared by the top-of-`send` guard, the
`wait:` send arm and this loop, so all three stay byte-identical (same pattern as
`FULL_SEND_DEADLOCK`).

**Deliberate engine divergence, stated.** `--serial` still faults `FULL_SEND_DEADLOCK` here. The
reason is that its drain runs queued jobs **one at a time and cannot interleave them** (decision D3,
queue-at-`submit`): `blocker` runs first and faults on the full channel before `closer` ever gets a
turn. Note what is *not* true — an earlier draft said "the closer does not exist yet"; it does exist,
it is queued, and it runs and closes the channel right after the fault (visible with prints, its
output suppressed by fault ordering). The engine simply cannot express a program that needs two jobs
alive at once. M:N now matches Go; `--serial` keeps its own answer until §2b removes it. Correctness
outranks engine agreement.

Regression tests (both M:N-only, both mutation-verified):
`eager_send_blocked_on_a_full_channel_faults_when_the_channel_is_closed` — stubbing the check out
makes it hang to the 30 s guard — and the ordering fence above. The first test synchronises on a
`ready` channel before closing, which is load-bearing: with the closer merely sleeping, a schedule
that runs it first makes `ch.send(1)` fault at the *pre-existing* top-of-`send` guard, and every
assertion passes on the UNFIXED binary for an unrelated reason.

**Untested residual of this fix:** nothing fences the no-hot-spin property. `cargo test` measures
verdicts, not CPU, so a future change that made the predicate report ready while `enqueue_bounded`
kept refusing would burn a core and still pass green.

### W7-5d — a dead stdout cancelled sibling `Executor` jobs, in a shape that varied by thread count and across runs — **FIXED 2026-08-05**

Found adversarially reviewing this milestone's docs, not while implementing W7-5/W7-5c. The run-all
guarantee (every queued job runs; §8 above) is explicitly for an ORDINARY job fault. A **hard halt**
(`Vm::executor_hard_halt`) was a separate, unconditional kill switch — and it counted a fault raised
while stdout is dead as one, so a broken pipe took the rest of the queue with it.

**The repro.** `Executor()` + a `spew` job that prints until stdout dies + two marker jobs that write
files, `shutdown()`, piped through `head -1`. Marker files written, HEAD before the fix:

| engine | m1 | m2 |
|---|---|---|
| `--serial` | N | N |
| M:N `--threads=1` | N | N |
| M:N `--threads=2` | Y | **N on 2 of 3 runs, Y on the third** |
| M:N `--threads=3+` | Y | Y |

The ledger's original entry recorded only the two ends of that table (`--serial` neither, default M:N
both) and left the thread-starved case "unverified". Measuring it is what turned a documented
asymmetry into a **nondeterminism**, and that killed the alternative the entry proposed — "an
accepted-asymmetry test pinning what each engine actually does" was never available, because there
was no stable shape to pin. The same program with an ordinary `panic()` in place of the spew wrote
both markers at every thread count on both engines, which isolated the cancel trip as the sole cause.

**The ancestor that owns `Executor` semantics, measured on the same shape under `| head -1`:** CPython
`ThreadPoolExecutor` runs every submitted job at `max_workers` 1, 2 and 4 — both markers, and all
three writes of the multi-write variant below. A broken pipe kills the printer, not its siblings. (Go
has no executor; see the cost note at the end for what Go's answer actually is and why it is not the
model here.)

**The fix — TWO process-global reads, not one.** The first was found by the repro, the second by
adversarial review of the first fix.

```rust
// 1. src/vm/mod.rs — an ERROR-property predicate that read ambient state
pub(super) fn executor_hard_halt(err: &RuntimeError) -> bool {
    err.is_over_memory || err.is_timed_out   // was: || stream::out_dead_reason().is_some()
}

// 2. src/vm/call.rs — `invoke_native` (and the `Writer` arm) ran `stream_halt`, which reads the SAME
//    global, after EVERY native call. Now gated on this call having actually emitted to stdout:
let writes_before = self.stdout_writes;          // bumped by `Vm::emit_out_bytes` (streamed branch)
let ret = func(&mut host)…?;
if self.stdout_writes != writes_before && let Some(halt) = self.stream_halt(span) { … }
```

All four readers of (1) change behavior for free: `ReadyWorker::run_outcome`'s two `trip_cancel` arms
no longer fire on a dead-stdout fault, serial `shutdown`'s pop-loop no longer `break`s, and
`reduce_task_slots`' `first_hard_fault` precedence is unaffected — it was already a no-op for this
case, since a global term made *every* `Fault` arm set `first_hard_fault` at the first fault, i.e.
`first_hard_fault == first_fault`.

(2) is what made the first fix incomplete in a way its own test could not see. With only (1) landed, a
sibling job doing three `fs.atomic_write`s completed **only the first** — it never printed, but the
post-native halt check read the global and faulted it — and how many completed still varied with the
thread count and across runs at `--threads=2`. The one-marker test passed because a single-native job
lands its write *inside* the native, before the check. The comment at that site asserted "this only
ever fires for the print natives"; nothing made that true, and the counter delta now does. The same
gate also stops a dead **stdout** from faulting a write to a FILE-backed or `stderr()`-backed
`Writer`.

Post-fix: both markers on **21/21 runs** across `--serial` and `--threads=1/2/3/4/8`/default, and all
three writes on **15/15** across `--serial`/`--threads=1/2/4`/default. The `| head -1` contract is
intact — `rc=1` with `stdout closed (broken pipe)` on stderr. `--timeout` still buckets `TIMED-OUT`
against a spinning job.

Pinned by six tests in `tests/interactive.rs` —
`dead_stdout_does_not_cancel_sibling_executor_jobs_{mn,mn_one_thread,serial}` for (1) and
`dead_stdout_does_not_tear_a_multi_native_sibling_{mn,mn_one_thread,serial}` for (2), all asserting
`rc != 0` + the pipe message so they cannot pass with `stream_halt` deleted. A real closed pipe is
needed, so this is the documented Rust fallback; `tests/chz/stdlib/executor_drain_test.chz` can only
raise `panic()`. `--threads=1` is the load-bearing configuration — it is the one that fails again the
moment either gate becomes reachable.

**Three lessons.**

1. **A process-GLOBAL read inside an error-property predicate.** `executor_hard_halt(err)` answers
   "is this ERROR a hard halt", and `out_dead_reason().is_some()` says nothing about `err`. Once
   stdout died, every fault anywhere in the process — in an unrelated nursery, in an unrelated
   executor — silently reclassified. Grep for the shape: a predicate over a value that also reads
   ambient state. **It was in the codebase TWICE**, and fixing the first instance is what made the
   second observable.
2. **An asymmetry you have not measured at both extremes may be a nondeterminism.** The entry was
   written from two data points at the comfortable end of the range. One `--threads=1` run and three
   repeats at `--threads=2` changed both the diagnosis and the set of available fixes.
3. **A one-call fence proves one call.** The first test used markers that made exactly one native
   call each — the single shape where instance (2) is invisible, because the write lands inside the
   native before the check. When the contract is "the REST of this job still runs", the fence has to
   contain a "rest".

**Accepted cost, stated deliberately — and note the primitive.** It is `ex.submit`-only: a job that
never prints and never returns (`while true: j = j + 1`) used to die with the queue and now runs
forever under a GRACEFUL `shutdown()`, so `chezzi run x.chz | head -1` on that program hangs where it
exited in 4 ms. **`shutdown_now()` still kills it** — measured 54 ms on `--serial`/`--threads=1`/
default — because the loop back-edge IS a cancellation point and `shutdown_now` trips the per-core
cancel flag. So the residual is exactly "graceful means graceful": run-all promises every job runs,
and nothing short-circuits that any more. It is not a class of job that has become uncancellable.
A `parallel:`/`spawn` nursery is NOT affected — the same program under `spawn` still terminates
promptly on both engines (measured), because structured concurrency aborts siblings on ANY fault, by
design. (Which nursery siblings had already FINISHED when the abort lands stays scheduler-dependent —
this repro's markers complete on default M:N and not on `--serial`/`--threads=1`. That is inherent to
first-fault-aborts-everything, the same as Go's `errgroup` + `context`, and is not W7-5d.)
**CPython hangs on the identical `ThreadPoolExecutor` shape**
(measured: `timeout 8` expires), so this follows the owning ancestor. **Go exits** — but by taking
SIGPIPE on fd 1 and killing the whole process, a signal policy Chezzi deliberately does not adopt
(`Vm::stream_halt` records why: restoring SIGPIPE would break `std.net`'s EPIPE-as-an-error contract).
An earlier draft of this entry claimed "Go and CPython hang on it too" — half wrong, and caught by
running it.

### W7-5e — the `stdout_writes` gate rests on an unenforced invariant — **FIXED 2026-08-05**

**The fix:** `stream::write_out(vm: &mut Vm, b: &[u8])` — it takes the writing `Vm` and bumps
`vm.stdout_writes` itself, so the count and the queue push are ONE statement. The counter is still
per-`Vm`; only the *place it is incremented* moved, from `Vm::emit_out_bytes` into the one door every
streamed stdout write already went through. A native that wants to emit uncounted bytes now has no way
to spell itself: the call needs a `&mut Vm`, and having one is having the counter bumped. Verified by
writing the bypass — `super::stream::write_out(b)` in `fileio.rs` — and confirming it stops compiling
(`error[E0061]: argument #1 of type &mut vm::Vm is missing`); it compiles on the pre-fix tree. Zero
behavior change: `| head -1` on a 100 000-line print loop still exits at **4 ms, rc=1,
`stdout closed (broken pipe)`** at default M:N and `--threads=1/2/4`, and the 53 `tests/interactive.rs`
fences (four broken-pipe ones + the W7-5d `Executor` sibling test) are unchanged and green.

**The correction, and it is the point of keeping this entry.** The row below ruled out the whole
*direction* — "**Why not just move the counter into `stream::write_out`?** … it would then be
PROCESS-global" — and so ranked three fences that all work AROUND `write_out` instead. Only the
`static`-beside-`OUT` spelling of that move is process-global. A move that **carries the `Vm`** is
per-`Vm`, and is smaller than every fence that was ranked above it: the filed (b) ("make `write_out`
private to `exec.rs`") is not expressible *at this file layout* — Rust has no friend visibility, and
`pub(in path)` only names an ANCESTOR module, so it cannot say "visible to my sibling `exec`";
`pub(super)` is already the tightest scope that lets `exec.rs` call into `stream.rs`. (Re-parenting
the file to `src/vm/exec/stream.rs` would express it, at the cost of moving the stream sink under the
dispatch loop — a worse home for it than taking the `&mut Vm`.) A rejected direction was carrying a
rejected *spelling*'s flaw, and the ranked alternatives inherited that.

The original filing follows.

W7-5d's second gate asks "did THIS native call emit to stdout" by taking a before/after delta of
`Vm::stdout_writes`, which only `Vm::emit_out_bytes` bumps (streamed branch). That is correct **only
while every streamed stdout write routes through that one method.** It does today — audited at the
time of the fix: `do_print`/`do_print_sep` (`stmt.rs`), the `print` builtin (`call.rs:~185`),
`VmHost::print` (`mod.rs:~3784`, the natives' own sink), `fileio.rs`'s stdout-backed `Writer`, and
`sched.rs`'s `pending_cancel_report`. Nothing enforces it.

**The failure if it breaks:** a new native that reaches `stream::write_out` by another path emits
bytes without bumping the counter, so its `stream_halt` never fires. `chezzi run x.chz | head -1` on a
loop calling that native spins forever, growing the unbounded stream queue (`stream.rs`'s documented
`ponytail:` ceiling) instead of exiting — the exact regression `6f8bb5c` and `ccbdadbc` were written
to prevent, on a new surface.

**Why not just move the counter into `stream::write_out`, where it would be enforced by
construction?** Because it would then be PROCESS-global, and the delta is read across a window in
which sibling OS threads are also printing: another job's write during my native call would fire MY
halt. That is precisely the cross-job contamination W7-5d exists to remove — the same class as the
`out_dead_reason()` term, re-introduced one layer down. Per-`Vm` is the correct shape.

**Cheapest fences, ranked:** (a) a `#[test]` asserting `stream::write_out` has exactly one caller
(grep-based, brittle but honest); (b) make `write_out` private to `exec.rs` so `emit_out_bytes` is
structurally the only door; (c) a `debug_assert` in `write_out` that the calling `Vm`'s counter moved
— needs a `&Vm`, which is why it is last. (b) is the real fix and is a visibility change, not logic.

*(End of the original filing. None of the three shipped — (b) is not expressible, and (a)/(c) are
proxies for what the signature now states outright. (c) came closest: it noticed `write_out` could
take a `&Vm`, then asked that reference to CHECK the counter rather than to be the one that moves it.)*

### W7-19 — `fs.stat` and `fs.walk` were the only filesystem syscalls that PIN a core worker — **FIXED 2026-08-05** (filed the same day by the `native::Kind` refactor, `future.md` §3c)

Converting the native registry to carry each member's `Kind` on its entry required writing down what
every member's classification is TODAY. Fifteen of `std.fs`'s seventeen members were in the old
`is_blocking` name list; **`_stat` and `_walk` never were.** So under the M:N engine they ran INLINE on
a core worker for the duration of their syscalls — and `walk` recurses an entire directory tree, making
it the worst offender of the set, not a marginal one. Same D5 starvation the list exists to prevent
(`docs/concurrency-tier-d.md`): while a `walk` ran, that worker served no other fiber.

**This is the silent failure `future.md` §3c predicted, found already in the tree.** Nothing errored,
no test went red — both were added after the list was written, and the list is a separate place. The
refactor **preserved the behaviour** (`Kind::Inline`, commented `BUG PRESERVED, NOT INTENT` at
`src/native/fs.rs`, fenced by `every_syscall_module_member_is_blocking` asserting these two were still
`Inline`, so the fix would have to come here and update it) rather than smuggling a behaviour change
into a pure refactor. That carve-out is what the fix below deleted.

**The ancestors both hand the worker off**, which is what makes this a bug rather than a policy choice:
Go's runtime releases the P when a goroutine enters a blocking syscall (`os.Stat`, `filepath.WalkDir`),
and CPython drops the GIL around `os.stat`/`os.walk`. In neither does one directory traversal stop the
rest of the program from being scheduled.

**The fix was one word per entry, and the proof was already in the tree.** `Kind::Blocking` requires the
off-heap-safety contract: primitive args in, primitive `NativeRet` out, no heap/stdio/os touch during
the call. Neither return shape is new to the boundary — `_walk`'s `Ok(List([Bytes…]))` is byte-for-byte
the shape `_list_dir` already offloads, and `_stat`'s `Ok(Struct{Int,Bool})` is the shape
`process.run`/`run_args` already offload (`process.cmd` is the `Ok(Str)` one — the wrong citation
survived into the first draft of this row and was caught in adversarial review). Both take their path argument through `arg_path` → `Host::arg_bytes`, which
`OffloadHost` serves from a pre-extracted `NativeArg::Bytes`; neither calls a `Host` I/O or os method,
so none of `OffloadHost`'s `unreachable!` arms is reachable. Return lowering (`Vm::lower_native`, which
resolves the `FileInfo` struct name) runs on the resuming worker **with** the `Vm`, identically for an
inline and an offloaded call. `walk_into`'s per-depth recursion is stack-safe because the blocking pool
uses the same `VM_STACK_BYTES` as an `MnSched` worker.

**Measured, `CHEZZI_THREADS=1`, release binaries built from the SAME commit with only these two
entries differing** — a scratch worktree with `Kind::Inline` restored and its own `CARGO_TARGET_DIR`,
runs interleaved pre/post so machine load cannot land on one side (the first pass compared against a
2-commit-older `main` and reported systematically smaller numbers on a quieter machine — same
direction, wrong magnitudes, and not a controlled comparison):

| 121 291-entry tree | pre-fix | post-fix |
|---|---|---|
| worst scheduling gap seen by a sibling fiber ticking every 5 ms | **136, 139, 138 ms** | **41, 39, 38 ms** |
| 4 concurrent `fs.walk`s, wall clock | **818, 825, 814 ms** | **455, 449, 469 ms** |

The 4-walk row is the D5 shape the filing asked for (cf. 4×`process.cmd("sleep 0.3")` = 305 ms offloaded
vs 1209 ms serialized); the ratio is smaller — 1.8× rather than 4× — because a page-cache-warm walk is
part CPU, not a pure sleep.

**The residual is the interesting part: offloading moves the syscall, never the allocation.** ~39 ms,
not ~5 ms, because lowering 121 291 paths into heap objects needs the `Vm` and therefore runs on the
core worker after the pool thread finishes. It scales with the RESULT, not the syscall: the same
program on an 18 661-entry tree gives 44–50 ms pre-fix → **9–10 ms** post-fix. Any future "why is this
native still stalling a worker?" should check the size of what it returns before doubting its `Kind`.

**Fences, and which one actually pins what** — worth being exact, because the first draft of this row
was not. **The classification fence is the Rust test**: `every_syscall_module_member_is_blocking` is now
exception-free (the carve-out that pinned the old state is gone, which is where the gap said the fix
must land), and it is the only thing that goes red if either entry reverts to `Kind::Inline`. The
`tests/chz` case `stat_survives_the_offload_boundary` is a **correctness** fence on the offloaded round
trip, not a classification one — it passes under either `Kind` (measured), since an inline call returns
the same values; what it covers is that `_stat`'s `Ok(Struct{…})` survives arg-extraction + off-heap
execution + lowering, which nothing else exercises (top-level code has `mn == None` and never offloads).
`_walk` needs no new case: the pre-existing `path_crosses_a_spawn_airlock` already walks from a fiber.
Both dual-engine gated. One intended side effect: `kind.blocks()` also makes a native an entry cancellation checkpoint on
both engines, so a cancelled fiber calling `fs.stat`/`fs.walk` now faults `cancelled` instead of running
the syscall — exactly what the other fifteen `std.fs` members already did.

### W7-20 — FFI writes to fd 1 are invisible to the broken-pipe halt, so `| head -1` never ends — **CLOSED 2026-08-05, NOT A BUG: both ancestors do the same. Documented**

Every stdout path the VM OWNS is now counted and halted (`W7-5d` + `W7-5e`). FFI does not go through
any of them — it calls the C function, which writes the descriptor itself:

```chezzi
extern "libc.so.6":
    fn puts(s: str) -> int

fn main():
    while true:
        _ := puts("line")

main()
```

| `chezzi run x.chz \| head -1` | result |
|---|---|
| the loop above (`puts`) | **6002 ms, killed by an external `timeout 6`, rc=0, no fault, no diagnostic** |
| the same loop using `print` | **3 ms, rc=1, `stdout closed (broken pipe)`** |

libc's `write(2)` returns `EPIPE` to *libc*, not to us: nothing sets `OUT_DEAD`, `out_dead_reason()`
stays `None`, and `stream_halt` has nothing to report. The counter is not the missing piece — a write
the VM never performed cannot be counted by any shape of counter. Nor does the OS catch it: Rust's
runtime sets SIGPIPE to `SIG_IGN` process-wide at startup, and the loaded C library inherits that
disposition, so FFI gets neither the signal a C program would die from nor the fault a Chezzi `print`
raises.

**Not a W7-5e regression** — it reproduces identically on the pre-fix tree, and no VM code path
changed. Filed because W7-5e's fix is what makes it the LAST uncounted stdout door, and because the
first draft of that fix's write-up claimed "a write the halt cannot see does not compile" without this
qualifier.

**RESOLVED by measuring the ancestors: this is the behaviour, not a defect.** The filing above said to
decide the contract before spending anything. Both owning ancestors were then run, and both do exactly
what Chezzi does — on **both** observables, not just the halt:

| loop under `\| head -1` | native print | the same loop through C |
|---|---|---|
| **Chezzi** (`print` / `extern` `puts`) | 4 ms, `stdout closed (broken pipe)` | **spins** — 6002 ms, killed by `timeout 6`, rc=0 |
| **CPython** (`print` / `ctypes` `libc.puts`) | 37 ms, `BrokenPipeError` | **spins** — 6001 ms, killed |
| **Go** (`fmt.Println` / cgo `C.puts`) | 2 ms, SIGPIPE | **spins** — 6001 ms, killed |

Ordering is identical to CPython too, and `io.flush()` does not change it (3/3 runs) — the C library's
own block buffering holds the FFI bytes until exit, so they land *after* everything the VM printed:

```
Chezzi:  chezzi-1  chezzi-3  ffi-2  ffi-4
CPython: py-1      py-3      ffi-2  ffi-4
```

**The correction is to the paragraph this replaces, and it is worth keeping.** It ranked the two
options as "flag the fd-1 writers" (the thorough one) vs "leave it and document" (*"cheaper and
narrower"*) — i.e. documenting was the budget choice. **The measurement inverts that.** The flagger is
not the better option bought with more effort; it is the *wrong* one, because it would move Chezzi away
from both ancestors on a surface where it currently matches them exactly. It is also incomplete by
construction — the fd-1 writers are unbounded (any C function can wrap `puts`), so a symbol list buys a
false sense of coverage. "Cheaper" was doing the arguing; nobody had run `ctypes`.

**What the measurement added to the docs — after adversarial review corrected it.** The C function's
own return value IS a working error channel. The first draft of this entry claimed the opposite for
buffered stdio (*"`puts` + `fflush(NULL)` never reports — glibc drops the per-stream error"*). **That
was false, and the bug was in the extern declaration, not in glibc:**

| `extern` declaration | `puts`'s value once the reader is gone | `if r < 0` |
|---|---|---|
| `fn puts(s: str) -> int` | `4294967295` | **never fires** — 200 000 iterations |
| `fn puts(s: str) -> int32` | `-1` | fires at **i=1638**, 3/3 runs |

A bare `int` marshals as C **`long`** (`syntax.md` §12b, fixed-width ints); `puts` returns a C `int`,
so the sign is lost and every guard silently dies. Two independent prosecutors caught it by running
the documented snippet — the doc had generalized one wrong-width example into a defamatory claim about
the C library. Detection is also deterministic once the width is right (i=1638 every run): the failure
surfaces when the 4 KiB stdio buffer first reaches `write(2)`, not on the call that filled it. And the
corrected number *strengthens* the ancestor match rather than weakening it — CPython's `ctypes`
detects at **i=1638** too.

**The lesson is the shape, not the width.** A doc that says "the library does not report X" is a claim
about someone else's code, made from one local observation; the honest version of that sentence is
almost always "my call did not observe X." Documented in `docs/syntax.md` §12b (contract + the
width-vs-detection table + one runnable example), with cross-references from `docs/stdlib.md`'s
`std.ffi` unsafe-contract blockquote and its `print`/stdout guarantee list — that list now ends by
saying it covers the VM's own sink only.

**Not fixed in code, deliberately, and nothing here is fenced by a test.** There is no behaviour to
regress, and the only assertable shape is a hang — a test that must time out to pass. Should this ever
be revisited, the trigger is not "FFI can write fd 1" (it always can) but CPython or Go changing what
*they* do.

### W7-22 — every container crossing the airlock is rebuilt at 22× capacity (Rust's in-place `collect` retains the wire buffer) — **FIXED 2026-08-06** (found while re-deriving `W6-10s`)

```chezzi
# 50 spawns of one 200 000-int list — peak RSS 3.45 GB before, 203 MB after
test fn spawn_args():
    blob: List[int] = []
    for i in range(200000):
        blob.push(i)
    parallel:
        for i in range(50):
            spawn use(blob)

fn use(xs: List[int]) -> int:
    return xs.len()
```

**Root cause.** `Vm::from_wire_memo`'s container arms all had the shape

```rust
let cloned: Vec<Value> = items.into_iter().map(|x| self.from_wire_memo(x, rebuild)).collect();
```

`Vec<T>::into_iter().map(f).collect::<Vec<U>>()` is specialized by the standard library into an
**in-place** collect when `size_of::<U>() <= size_of::<T>()` and the alignments allow: the source
`Vec`'s allocation is written through and handed back as the destination `Vec`, whose capacity is
therefore `src_capacity * size_of::<T>() / size_of::<U>()`. Here `T = WireValue` (176 B) and
`U = Value` (8 B), so **every rebuilt container came out at 22× the capacity it needed** and held the
whole wire buffer alive for as long as the object lived.

Measured on the release binary, instrumenting `sweep()` to dump live objects over 100 KB:

| heap | `len` | `capacity` | `Obj::List` bytes |
|---|---|---|---|
| parent (the original `blob`) | 200 000 | 200 000 | 1 600 000 |
| worker (the rebuilt `spawn` arg) | 200 000 | **4 400 000** | **35 200 000** |

Halving the list to 100 000 gave `capacity = 2 200 000` — 22× again, deterministically. Peak RSS for
the program above, same program on both binaries: **3 450 096 kB → 202 776 kB**.

Reach: **fourteen sites**, not the eight the first cut found. `from_wire_memo`'s eight container arms
— `List`, `Tuple`, `Iter`, `Struct`, `Enum`, `Closure` captures, and both generator arms (`Pending`
args, `Suspended` stack) — plus `deep_clone_all` (one) and `rebuild_ready`'s five `Lowered` arms.
Two of those six extras are DURABLE, not transient: `deep_clone_all`'s result and
`Lowered::Closure`'s captures both land in an `Obj::Closure { captured }` that lives as long as the
closure, so the `parallel:`/`spawn` capture path leaked 22–24× even after the first cut — measured
`len 4096 → capacity 98304` for the keyed `Vec<(Box<str>, WireValue)>` (192 B / 8 B = 24×).
`Map`/`Set` were
already safe: they rebuild through explicit `push` loops (they carry the hash alongside), which is
exactly the shape the fix generalizes. The `to_wire` direction is safe by construction — the element
GROWS (8 → 176), so the in-place specialization cannot apply — and `replay_snap`'s list arm uses
`.iter()`, not `.into_iter()`.

**Fix.** One helper, `Vm::rebuild_items`, used by all fourteen sites. It pre-sizes with
`Vec::with_capacity(items.len())` and pushes, so the capacity is exact; a `pick` closure pulls the
`WireValue` out of each element so a keyed list (`Vec<(Box<str>, WireValue)>` — struct fields, closure
captures) shares the one path. Cost: one extra live buffer while the source drains, which the wire
copy was already paying; `shrink_to_fit()` after the fact was rejected because it reallocs and memcpys
the whole container on every `recv`/`spawn` to undo something that should not have happened.

**Tests.** `vm::gc_tests::a_crossed_container_rebuilds_at_exact_capacity` asserts `capacity == len`
for a `List`, a `Tuple` and an `Iter` round-tripped through `to_wire`/`from_wire`;
`vm::gc_tests::deep_clone_all_rebuilds_at_exact_capacity` fences the second, separately-missed family.
Both mutation-verified: restoring the `collect()` shape turns them red with `rebuilt at 90112 capacity
for 4096 elements` and `deep_clone_all returned 90112 capacity for 4096 values` — 22× exactly — while
the rest of the suite stays green, which is the point. **No value-level test can fence this**: `len`,
element order, contents and printed output are all identical either way, so `Vec::capacity` is the
only observable. That is why 3856 green tests never caught it.

**Not moved:** `benches/run.chz` is single-threaded and never crosses the airlock, so it prices none
of this — the bench set has no coverage of airlock memory behaviour at all. Re-measured after the fix
anyway (fib 2.84×, loop 1.03×, str 1.93×, primes 2.09×, list 2.25×, struct 2.49×, poly_method 3.89×,
map 1.75×, empty 4.61× faster): unchanged, as expected for a path they do not touch.

### W7-21 — a module global holding a FN VALUE cannot be CALLED through the module (`m.G` resolves, `m.G()` does not) — **FIXED 2026-08-05** (filed the same day while building a W7-4a repro)

```chezzi
# k.chz
fn one() -> int:
    return 1
# l.chz
import k
BARE := k.one            # a module global whose TYPE is a fn

# main.chz
import l
x := l.BARE              # ok: no type errors
y := l.BARE()            # type error (line 2, col 6): module 'l' has no member 'BARE'
z := l.BARE
w := z()                 # ok — binding it first works
```

**Both ancestors accept it, measured:** CPython `m.G()` where `G = _one` → `1`; Go `pkg.G()` where
`var G = one` → `1`. Nothing about the two-step spelling is more correct, so this is drift, not design.

**Root cause — the two member lookups read DIFFERENT maps.** `ModuleSig` (`src/checker/mod.rs:607`)
carries `functions: HashMap<String, FnSig>` **and** `values: HashMap<String, Ty>`. A declared `fn` lands
in `functions`; a top-level `let`/`:=` binding lands in `values`, whatever its type. The VALUE path
reads `values`, so `l.BARE` resolves. The CALL path (`src/checker/expr.rs:2182`, the `Ty::Module(mname)`
arm of the call inference) reads **only** `functions`:

```rust
let fsig = sig.and_then(|s| s.functions.get(method).cloned());
…
self.error(span, format!("module '{mname}' has no member '{method}'"));
```

so a `values` entry of type `Ty::Fn` is never consulted and falls straight to the diagnostic — which
then *lies*: the member exists, it just isn't a declared `fn`.

**Shape of the fix**: on the `fsig == None` path, before erroring, look the name up in `values` and, if
its `Ty` is a `Ty::Fn`, check the args against it and return its result type — the same fallback the
value path already performs. Note the diagnostic is wrong independently of the fix and should say
something truthful when the name IS present but is not callable.

**Scope, checked**: not destructuring-specific (that was the red herring the repro started from —
`D1, D2 := k.fns()` fails for the same reason a plain `BARE :=` does), not nested-fn-specific, and the
element type is irrelevant (`(int, int)` and `(List[int], List[int])` halves are fine because nobody
calls them). The trigger is exactly *call syntax on a `values` member of fn type*.

**Class**: `checker⊋compiler`'s sibling — a checker that REJECTS what the rest of the system supports
(the value path proves the binding exists and the two-step call runs). Also single-module-clean:
`chezzi check l.chz` alone says `ok`, because the failure needs an importer. Same shape as the
`checker test helper key divergence` and `reserved method table: two harvest paths` notes — a member
surface harvested into two places, with one consumer reading only one of them.

**FIX (2026-08-05) — checker-only, `src/checker/expr.rs`.** The `Ty::Module` call arm now also clones
the same name out of `sig.values` and, on the `fsig == None` path, calls through it when its `Ty` is a
`Func`/`BuiltinFn` (STRICT `check_args` — no int→float widening through a function value, the same
rule the fn-value `expr.rs:533` and fn-field `expr.rs:2362` paths already carry). Compiler and VM are
UNTOUCHED: `l.BARE(…)` already lowered to the ordinary `Op::CallMethod` fall-through
(`compiler/mod.rs:4388`), and `Obj::Module` dispatch (`vm/call.rs:1278`) already looks the member up in
the module's slot table and `do_call`s whatever value is there — closure or native alike.

Measured on the 3-file repro above, release binary, before → after:

| | before | after |
|---|---|---|
| `chezzi check main.chz` | `type error (line 3, col 6): module 'l' has no member 'BARE'`, rc=1 | `ok: no type errors`, rc=0 |
| `chezzi run main.chz` (M:N) / `--threads=1` / `--serial` | never reached | `1` on all three |
| ancestors, re-run | CPython `pk.G()` → `1`; Go `pkg.G()` → `1` | Chezzi now agrees |

The lying diagnostic is fixed independently: a member that exists but is not callable now reports
`module 'l' member 'N' is not callable (it has type int)`, and a genuinely absent member keeps
`module 'l' has no member 'NOPE'`.

**Two things adversarial review added.** (1) A member whose own initializer already errored
(`X := k.nope`) is `Unknown`-typed, and the first cut reported it as *"not callable (it has type ?)"*
— a cascade asserting a type nobody knows. It now stays SILENT (the checker's `Ty::Unknown`
suppression convention), so that program went **2 errors → 1**, matching what the two-step spelling
`f := l.X; f()` already reported. Note this was NOT a regression — pre-fix the same program also
emitted 2 (the second being `has no member 'X'`, measured on a rebuilt pre-fix binary) — the fix just
had no reason to keep it. (2) The new arm records the editor HOVER for the member name, which the
filing had written off as out-of-scope because `record_method_hover` takes an `FnSig` a `values`
member lacks: the member's own `Ty::Func` **is** what that helper builds from an `FnSig`, so it is one
`hover_record_at` call, fenced by `editor::tests::hover_module_fn_value_member_call` (verified to FAIL
with the line removed). "The helper doesn't fit" was a statement about the helper, not the feature.

Also from review, and worth keeping: the STRICT-vs-widening choice was **claimed by a comment and
pinned by no test** — the two original cases (arity, `str` into `int`) fail under either helper. The
deciding case is an int literal into a `float` param: `l.FL(2)` where `FL := k.half` errors
`expected float, found int`, while the DECLARED spelling `k.half(2)` widens. Both are now asserted.

Fences: `checker::tests::module_global_of_fn_type_is_callable_qualified` (+ arity/type-mismatch and
both diagnostics), and `vm::tests::module_global_fn_value_call_runs_both_engines`. **The VM test is
not the fence and its doc-comment says so** — `run_file` does not run the checker, so it passes
pre-fix; what it locks is the other half of the claim, that the accepted form really executes, byte
-identically on both engines.

**Lesson: the runtime half of a `checker⊋compiler`-family finding needs a different kind of check
than the checker half.** The natural instinct here was "run it on both engines and we're done" — but
that test was green *before* the fix, because the VM helpers bypass the checker entirely. A both
-engine run proves the lowering exists; only a graph-level `check_graph` test proves the rejection is
gone. Two tests, two different claims, and neither substitutes for the other.

**Not in scope** (checked, unchanged): keyword args through a module fn value (`l.BARE(x=1)`) — the
desugar pass resolves `named` against the callee's params before the checker, so this arm never sees
them; and hover, since `record_method_hover` takes an `FnSig` a `values` member does not have (no
regression — there was no record before either). `from l import BARE` + `BARE()` already worked
(`setup.rs:1337` declares the bind as a `Ty::Func` value and the ordinary value-call path handles it),
which is what made the qualified arm the single broken site.

### Safe-direction observations (not filed as bugs)
- **`Vm::stream_halt`'s stated reason for never restoring SIGPIPE is weaker than it reads.** The
  comment says restoring it "would break `std.net`'s EPIPE-as-an-error contract", presenting the two
  as mutually exclusive. **Go has both**, because the split is by FILE DESCRIPTOR, not global —
  `$GOROOT/src/os/signal/doc.go` §SIGPIPE: "A write to a broken pipe on file descriptors 1 or 2
  (standard output or standard error) will cause the program to exit with a SIGPIPE signal. A write to
  a broken pipe on some other file descriptor will take no action on the SIGPIPE signal, and the write
  will fail with a `syscall.EPIPE` error… This means that, by default, command line programs will
  behave like typical Unix command line programs, while other programs will not crash with SIGPIPE
  when writing to a closed network connection." That is exactly Chezzi's own split (CLI stdout vs
  `std.net` sockets). Not proposing the change — the in-VM halt composes with `defer`/`recover:`/task
  joins in ways a signal cannot, and `chezzi` is also library code (`chezzi-lsp`, embedders) where
  killing the host process is not ours to do. But if W7-5d's graceful-`shutdown()` hang ever bites,
  fd-scoped SIGPIPE is a real option and the comment should not read as if it were ruled out.
- **`PROGRESS.md`'s claim that the second (rejected) W7-5 fix attempt (`8c32fda6`) broke the `os.exit`
  hard halt "0.006s → 18.9s" is very likely a misattribution.** That stall reproduces IDENTICALLY on
  pre-fix `main` (verified by rebuilding both binaries): a job blocked in `time.sleep_ms` never reaches
  a cancellation point, so the join waits the sleep out regardless of the cancel flag's state — the flag
  was never the variable that mattered for that measurement. With CPU-loop siblings instead of a sleep,
  the hard halt is `0.006s`/`rc=3` at every thread count, on every attempt. One of the four reasons this
  milestone was rejected twice was measuring sleep-cancellability (a pre-existing, unrelated limit — a
  blocking OS sleep has no in-flight cancellation point once entered) rather than the kill switch the
  attempt was actually supposed to be judged on.
- **Serial's hand-written mirror `Vm::drain_cancelled_children` (`src/vm/sched.rs:1925`) lacks the
  hard-halt precedence Task 1 added to `reduce_task_slots`.** Unreachable today — after W7-5d the only
  hard-halt markers left are `--max-heap` and `--timeout`, and both are M:N-only (`src/main.rs:685`,
  `:693`) — but it becomes a real serial-vs-M:N divergence the moment any serial-observable hard-halt
  marker is added. (W7-5d strengthens this: the observation used to lean partly on the dead-stdout
  marker being "uniform across all fault kinds", which was true only because that term was a process
  global; it is gone now, so the *only* thing keeping this unreachable is the M:N-only gate.) Worth a
  grep-and-mirror pass if that ever happens; not worth pre-emptively duplicating logic for a marker
  that doesn't exist yet.
- **`std/cancel.chz` — `kids` only ever grows.** `derive()` registers a child into every ancestor's
  `kids` list and nothing ever unlinks it. Go's `context.WithCancel` returns a `CancelFunc` precisely so
  `defer cancel()` detaches the child from its parent; there is no detach here at all. A long-lived root
  token with a token derived per job retains one channel per job forever, plus an O(depth) `update()`
  read-modify-write lock per `derive()` call as the tree grows. Narrow trigger today (nothing yet derives
  tokens at volume against one long-lived root), but a real leak shape once something does.
- **`std/cancel.chz` — `cancelled()` recurses the parent chain at POLL time** (one `Shared.get()` per
  level plus a `monotonic()` call), while Go pushes cancellation DOWN at `cancel()` time so `ctx.Done()`
  is a single channel read. The push machinery already exists here — `cancel()` already fans out to
  `kids` — it just carries a `Channel[bool]` wakeup rather than also writing the child's own `flag`.
  Storing the child's `Shared[bool]` in the parent's registry too (not just the wakeup channel) would
  collapse `cancelled()` to one local read, cheaper and less code than the current recursive walk.

### W7-12r / W7-15 — the process-wide quiescence detector (`future.md` §2d **step 0**) — **FIXED 2026-08-04**

Closes every residual of W7-12's interim predicate, and one wrong answer that predicate never reached.

**Measured first, on built binaries, before any code was written** (Go 1.26 compiled, CPython 3, and
`target/release/chezzi` at `8c401e8f`, all under a `timeout`). Go is the concurrency ancestor and so
the baseline for the deadlock VERDICT; `Executor` itself is Python/Java lineage. `--serial` is not
consulted — it is scheduled for removal (`future.md` §2b) and a doomed engine cannot be a standard of
correct.

| program | Go | CPython | Chezzi before | Chezzi after |
|---|---|---|---|---|
| (a) two jobs of one executor on an empty `recv`, `shutdown()` | `all goroutines are asleep` (rc 2) | hangs | **hangs** | **faults, 9 ms** |
| (b) two executors deadlocking each other | `all goroutines are asleep` (rc 2) | hangs | **hangs** | **faults, 9 ms** |
| (c) one blocked job, no `shutdown()` (exit drain) | rc 0 — **abandons** the goroutine | hangs | **hangs** | **faults, 4 ms** |
| (d) = **W7-15**: `main` `recv`s while an eager job `send`s | `main got 42` | `main got 42` | **faults** | `main got 42` |
| cap-1 pipeline, 50 handoffs (health fence) | completes | completes | completes | completes, **0/40 false faults** |
| producer in ANOTHER executor (health fence) | `got 1` | `got 1` | `got 1` | `got 1` |
| `shutdown()` in a `spawn:` beside a live producer (health fence) | `job got 42` | — | `job got 42` | `job got 42` |

Two things follow. (c) is **not** a measured gap against either ancestor — Go abandons the goroutine at
`main`'s return, CPython's `ThreadPoolExecutor` joins its non-daemon threads and hangs. Chezzi joins at
exit (decision D1, CPython's model), so pairing CPython's join with Go's verdict rule is stricter than
both, deliberately. And (d) — filed here as **W7-15**, previously unrecorded — was a WRONG ANSWER
rather than a hang, which by this project's own bar outranks (a)–(c) put together.

**The rule.** Go's own detector, with one adaptation:

> deadlock ⇔ every counted party is registered as blocked **AND** no registered party's wait condition
> is already satisfiable.

The second clause is the adaptation, and it is what every counter-only attempt lacked. A bounded cap-1
pipeline is permanently "all parties parked" while perfectly healthy — but with `cap == 1` the channel
is either non-empty (the parked RECEIVER is satisfiable) or has a free slot (the parked SENDER is), and
never neither. Two jobs on a genuinely empty channel are both unsatisfiable. **Satisfiability separates
parked from unfeedable**, which no progress counter or debounce window could (see W7-12's rejected
experiment above, and the `parked-is-not-stuck` memory).

**Counted parties, and why the count is sound.** `live = 1 (main) + Σ ExecutorCore::outstanding` over
the run's `ExecRegistry` — no new counter, deliberately: `outstanding` is bumped at `reserve()` (at
`submit`, before dispatch) and dropped at `finish()`, by the code that owns job lifetime. A thread that
is NOT a counted party — an `MnSched` worker, netpoller/timer callback, blocking-pool thread — can only
run user code while some counted party sits inside a nursery or a native call, and such a party is live
and unregistered, so `blocked < live` vetoes. **An uncounted sender therefore always implies a veto**,
which is why no "is a scheduler alive?" global is needed and why `MnSched::is_deadlocked` was left
completely alone, with every veto it earned intact. `Vm::is_counted_party` is the corollary: register
only with no scheduler of any kind and no native-callback frame.

The error directions are asymmetric and all fall the safe way — a missed registration or an
over-generous satisfiability check costs a HANG; only an under-count of `live` could fault a live
program, and that is the one quantity not hand-maintained.

**What it deleted.** `Vm::eager_join_deadlocked`, `Vm::join_has_no_live_siblings`,
`ExecutorCore::joining`/`blocked`, `JoinGuard`, `BlockGuard`, `Vm::eager_block_suspect` and its
two-observation debounce, and the registry sweep — the whole interim predicate, not a layer over it.
`Vm::join_eager_jobs` registers a `PartyWait::Join` instead, for EVERY join: the explicit `shutdown()`
and the program-exit drain alike, which is what closes residual (c). W7-12 could not arm its guard at
the drain because a per-executor verdict would have let registry ORDER decide whose job faulted; a
process-wide verdict has no such ordering problem.

**Five bugs found while building it, each a lesson for §2d steps 1–4.** The first three were caught by
an existing looping fence — `an_eager_wait_block_is_woken_by_its_arm_not_by_the_poll_timeout`, a
300-handoff gate/data pipeline — and the last two by `adversarial-review`, on a change whose whole
gate was already green. None by reasoning:

1. **A party must not stay registered across its own retry.** `pop()` and un-registering are not one
   atomic step, so a party still registered while it consumes a value reads as *parked at the very
   instant it made progress*. Registration is now scoped to the wait, never the attempt. Faulted 6/10.
2. **The verdict must be ONE observation.** The first cut cloned the party list, released the lock, then
   read the channels — so it judged channel states against a party set that never existed at any single
   instant (a party can register, be fed, un-register and run on while the stale clone still names it).
   It reported a producer parked on `gate` and a consumer parked on `data` with both empty — a state
   that program cannot reach, since whichever parked second must have fed the other first. The party
   lock is now held across the whole verdict. Lock order is `parties` → `exec_registry`/`eager` →
   `ChannelCore::q`, and nothing anywhere takes `parties` while holding a channel or executor lock.
3. **Satisfiability replaced the debounce, and that is a semantic upgrade, not a tuning one.** "Is this
   wait already over?" has a direct answer; "has nothing moved recently?" is a guess, and it is the
   guess that faulted a healthy pipeline 6/40 runs.
4. **`closed` means OPPOSITE things at the two recv sites, and folding them was a HANG regression.** A
   single `recv` on a closed channel makes progress (`ClosedEmpty` — the `for` ends, a bare `recv`
   faults), but the `wait:` poll *SKIPS* a closed+empty recv arm (W7-13r(a)), so that arm is not
   progress at all. One `PartyWait::Recv` covering both made a closed arm answer "satisfiable"
   forever, vetoing the verdict permanently: `c1.close(); wait: c1.recv() / c2.recv()` faulted in 0 ms
   before this detector and hung after (measured rc 1 → rc 124, both engines). Split into `Recv` and
   `Wait`. **The general rule, and it is W7-13r(a)'s rule re-learned in a new place: a satisfiability
   arm must mirror what its site actually SETTLES on, condition for condition — not what changed.**
5. **A wait predicate that answers a CONSTANT is a bug waiting for a window.** `PartyWait::Join`
   answered a flat "never satisfiable", which is wrong for an already-drained join: `join_eager_jobs`
   registers before it can take the executor lock, so `Executor(); e.shutdown()` — and the whole
   window while the last job's `finish` wakes a real joiner — left a permanently-unsatisfiable party
   in the registry for a thread about to return and keep running. A sibling sampling there faulted a
   LIVE program, 2/20 runs. A join's condition is `outstanding() == 0`; it now answers that. Fenced by
   `a_drained_shutdown_is_not_mistaken_for_a_blocked_joiner` (15 runs per invocation — the mutation
   fails it 3/3, where the CLI shape showed the bug only 2/20).

**Residuals, stated deliberately.**
* **A cycle made only of joiners hangs.** A joiner never self-faults (the jobs it waits for do), and it
  waits untimed, so a hypothetical cycle in which *every* party is a `Join` has nobody to form the
  verdict. Needs `main` → job → executor → job → executor → `main`, all joins and no channel block.
  Rare enough to decline rather than add a polling joiner and a fault message for it.
* **Bounded-pool starvation hangs, and it bounds two of the acceptance tests.** A job reserved but
  never dispatched (pool full of blocked jobs) counts live forever, so the verdict declines — correct
  by the error-direction table, and it hung before too (`pool.rs` risk G3, pre-existing). Measured:
  shapes (a) and (b) fault at `--threads=4` and hang at `--threads=1`, because the second job never
  gets a thread. So `two_blocked_jobs_in_one_executor_fault_instead_of_hanging` and
  `two_executors_deadlocking_each_other_fault` **need ≥2 free pool threads** and say so in their
  watchdog messages; a single-core host would see them time out. Shapes (c) and (d) are fine at one
  thread.
* **Partial deadlock is still out of reach** — a subset stuck while the rest of the program runs on.
  That is §2d step 3 (AND-OR knot detection) and always was; step 0 buys TOTAL quiescence only, exactly
  as Go's own rule does.
* **A live nursery is covered only indirectly**, by its owner being an unregistered live party. Folding
  scheduler parties into the same registry is §2d step 2.
* **`--serial` keeps its old answer on all four shapes** (it queues at `submit`, decision D3, so no job
  can run before `main` blocks). Recorded as a §2b-pending artifact, not a contract: the acceptance
  tests are M:N-only and say so.

* **The verdict is O(parties + executors + arms) under one global lock, per blocked party, per 5 ms
  tick.** Accepted: it runs only on threads that are already blocked, never on a hot path, and the
  single lock is a correctness requirement (lesson 2 above), not an oversight. `exec_registry` is
  push-only, so a program that constructs very many executors and then blocks pays for all of them —
  the same push-only bound `ExecRegistry` already documents.

**Acceptance** (`src/vm/tests.rs`, all watchdogged — the failure mode is a hang — and all
mutation-verified: stubbing `quiesced()` to `false` fails the first three, and restoring the old
`eager_core.is_some()` recv gate fails the fourth):
`two_blocked_jobs_in_one_executor_fault_instead_of_hanging`,
`two_executors_deadlocking_each_other_fault`,
`blocked_job_with_no_shutdown_faults_at_the_exit_drain`,
`main_recv_completes_when_an_eager_job_sends`, plus the two review regressions
`a_wait_with_a_closed_arm_still_reports_the_deadlock` and
`a_drained_shutdown_is_not_mistaken_for_a_blocked_joiner`. The health fences
`executor_bounded_pipeline_is_not_mistaken_for_a_deadlock` (re-run 40× on the CLI: 0 false faults),
`executor_job_keeps_waiting_when_shutdown_runs_beside_a_live_producer`,
`executor_job_keeps_waiting_while_another_executor_still_owes_work`,
`executor_job_blocking_recv_waits_for_a_later_send` and
`executor_job_blocked_during_shutdown_faults_both_engines` all stay green unchanged, as does
`tests/chz/stdlib/executor_drain_test.chz` (frozen by decision D5).

---

## Session log — 2026-08-06 (protocol embeds: flattened at every use site — the `:64` row, FIXED, plus one defect it never named)

The `protocol embeds` row had sat untriaged since bug-hunt wave 3, filed as a *safe-direction
observation*: `spec.md` said an embed set is "flattened at bound sites", the checker disagreed, and
nobody had decided which was wrong. Measuring against the owning ancestors settles it — the docs were
right.

### Measured, on the release binary, before the fix

| case | Chezzi | Go | Python / pyright |
|---|---|---|---|
| `p: Person` → `p.name()` (embedded) | ✗ `type Person has no method 'name'` | ✓ `ada 36` | ✓ 0 errors |
| `[T: Person]` → `p.name()` | ✗ `type parameter T has no method 'name'` | ✓ `eve 7` | ✓ |
| `Person` value → `Named` param | ✗ `expected Named, found Person` | ✓ `ada` | ✓ |
| `<` via embedded `Comparable` (bound) | ✗ `cannot compare T and T` | n/a | ✓ |
| `in` via embedded `Contains` (bound) | ✗ `cannot use \`in\` on T` | n/a | ✓ |
| `a + b` on `a, b: Vecish` (protocol declares `add` **itself**) | ✗ `cannot apply + to Vecish and Vecish` | n/a — Go bans `Self` in interfaces | ✓ (pyright — **and pyright is wrong**, see below) |
| `p.add(q)` where the sig is `fn add(self, o: Self) -> Self` | ✗ `expected Self, found Vecish` | n/a | ✓ (same) |
| **controls** — the same five with **own**, non-embedded methods | ✓ all pass | | |

Go reference (`type Person interface { Named; Age() int }`, a `Dev` witness, `show(p Person)`,
`onlyNamed(n Named)`, `viaBound[T Person]`): `ada 36` / `ada` / `eve 7`, rc=0. Python reference
(`class Person(Named, Protocol)`): runs, and `pyright` reports 0 errors / 0 warnings.

### The controls are what turned one report into two bugs

Running every case a second time with an **own** method in place of the embedded one is what split
the report. Rows 1–5 pass their control, so they are one defect: `ProtocolInfo.methods`
(`checker/mod.rs:586`) holds own methods only, and five consumer sites read it directly while
`flatten_embed_methods` (`proto.rs:359`) — which already existed and was already used by declare-time
validation and `protocol_has_static_method` — was never consulted from any of them.

Rows 6–7 **fail their control too**, so they looked like a second, independent defect. They are not
a defect at all — **they are the correct answer, reached by accident**, and the first cut of this fix
"fixed" them into a soundness hole. See the next section: rows 1–5 are the bug, rows 6–7 were never
one. The genuine second defect is narrower: `contains_item_ty` and
`index_kv`/`index_set_kv`/`slice_result` had no `Ty::Protocol` arm, so `in` / `[]` / `[a:b]` did not
reach an existential even where the method has no `Self` parameter, and `Self` in a RETURN position
leaked out of the existential method-call arm as the bare `Ty::Param("Self")` instead of widening to
the receiver.

### The fix

Checker-only, in all six places. `a.name()` lowers to name-keyed `Op::CallMethod` and `a + b` to a
type-blind `Op::Add` that dispatches on the runtime object, so every one of these already worked at
runtime — a protocol existential is erased, and the receiver is the concrete witness.

- **`Checker::protocol_method_sig`** (`proto.rs`) — own methods first, then transitively through the
  embeds, substituting each embed's args into the pulled-in signature. Deliberately NOT built on
  `flatten_embed_methods`, which does not substitute `Bound.args` (its four callers only inspect
  `is_static`, so it never mattered there). Wired into the existential and bound method-call arms,
  `protocol_has_method`, and `contains_item_ty`.
- **`Checker::protocol_provides`** — the `Ty`-level twin of `bound_provides`, so a protocol VALUE
  satisfies anything it transitively embeds. Plus a structural fallback (does `p` itself supply
  `protocol`'s methods?), which is what lets a `Vecish` declaring `add` witness the builtin `Add`.
  Arg matching stays strict throughout: `Container[str]` still does not satisfy `Container[int]`.
- **Object safety** (`self_in_param_position`, `checker/mod.rs`) — a requirement with `Self` in a
  non-receiver PARAMETER slot is un-witnessable by an existential. One guard at the `satisfies` root
  (so operator dispatch, `<`, and passing the value into a `[T: Add]` generic are all covered without
  their own copy) plus one in the existential method-call arm for the hand-written `a.add(b)` form.
  `op_overload_result` and `ordering_allowed` therefore get NO `Ty::Protocol` arm.
- **Operators that are object-safe** — `Ty::Protocol` arms in
  `index_kv`/`index_set_kv`/`slice_result`/`contains_item_ty` through one shared `protocol_op_sig`
  seam (`index(self, key: K) -> V` and friends have no `Self` parameter). Unary `-` needed no edit —
  it routes through `satisfies`, and `neg(self) -> Self` is object-safe. `Ty::Protocol` also had to
  be excluded from the `Iterator` and `Index`/`IndexSet`/`Slice` early-arms of `satisfies_args_d`,
  which errored on it before the protocol arm was ever reached.
- **`Self` in a RETURN ↦ the receiver** in the existential method-call arm, matching the `Ty::Param`
  arm — the half of the `Self` question that IS sound (it widens to the protocol).

### The bigger lesson: rows 6–7 were the checker being RIGHT, and the first fix broke it

The first cut of this change shipped **fully green** — `cargo test` 3848, clippy clean, both engines,
14 checker tests, 8 running `test fn`s — and had opened a check-OK-then-fault hole. Adversarial
review found it; two independent prosecutors filed the same charge from the same three-line program:

```chezzi
protocol Vecish:
    fn add(self, o: Self) -> Self
struct V: …fn add(self, o: V) -> V…      struct W: …fn add(self, o: W) -> W…
fn plus(a: Vecish, b: Vecish) -> Vecish:
    return a + b
print(plus(V(1), W("q")))
# check: ok  →  runtime error (line 6): no field 'x' on W(s=q)      ← BOTH engines
```

**A protocol value erases which witness it holds**, so two values of one protocol need not be the
same concrete type. Binding `Self` to the existential asserts they are. This is exactly Rust's
object-safety rule (a `Self`-typed parameter makes a trait non-`dyn`-able) and exactly why Go bans
`Self` from interfaces at all. Every operator protocol's method is `(self, Self) -> Self`, so
`+ - * / % <` are all bound-only on a value; the sound spelling — `fn plus[T: Vecish](a: T, b: T)` —
binds both operands to ONE witness and already worked, and correctly rejects `plus(V(1), W("q"))`.

**The ancestor check was run on the wrong ancestor.** pyright accepts the Python twin, and that was
taken as the licence. But Python's `Protocol` is gradual — it does not enforce witness identity at
all — so a pyright PASS is not evidence a statically-enforced language may accept it. The two
ancestors that can actually *express* the question both say no. "An ancestor accepts it" is only
evidence when that ancestor enforces the property in question.

Also introduced and caught by the same review, both fixed: (a) the embed walk had a DEPTH cap and no
visited set — with branching ≥ 2 that is 2^64 visits, so a diamond or cyclic embed graph hung `check`
(and the LSP) past 15 s on a method MISS, and the cycle diagnostic never printed; (b) the
conformance-witness half of `satisfies_args_d` still resolved embed args with a bare
`resolve_ty_ro`, so `b: PBag[str] = B` (whose `contains` takes `int`) was ACCEPTED — the read side
was re-spelled, the write side was not, and the read side's comment claimed the write side had
already witnessed it.

### Round 2 — the object-safety guard was itself mis-placed, and three more fell out

The fix for the above shipped green too, and a SECOND review round (2 prosecutors, fresh diff) found
the guard had been put at the `satisfies` root — which cannot tell a generic BOUND from a plain
annotation, so it netted every use of the value:

```chezzi
fn takes(p: Vecish) -> int: return 1
fn f(a: Vecish) -> int: return takes(a)
# type error: argument 1 of 'takes': expected Vecish, found Vecish     <- absurd, and pre-fix legal
```

A single pass/return/assign slot pairs NOTHING, so the guard's own premise never applied there. It
also hit every user protocol embedding `Add`/`Comparable`/`Arithmetic`. Moved to the actual pairing
sites: `enforce_bounds` (a generic type param, whose two slots could hold two witnesses), the
existential method-call arm, and the operator arms (which simply have no `Ty::Protocol` arm). Three
more, same round:

- The relocated guard read **own methods only**, so `protocol Vecish: Add` — where the `Self` method
  arrives through the EMBED, the commonest spelling — walked straight through it.
  `protocol_self_param_method` reads the flattened set.
- An embed arg naming an **undeclared** type (`protocol Bag: Contains[T]`, no `T` declared) resolved
  to `Ty::Unknown`, and an `Unknown` requirement accepts every operand: `"oops" in b` on a `Bag`
  whose `contains` takes `int` was `check: ok` → `cannot apply Add to str and int`. Now a hard error
  at the declaration — the same Unknown-as-permissive hazard as the nested case, reached by a typo.
- `protocol_op_sig` never bound `Self`, so `o[0]` on a `fn index(self, k: int) -> Self` protocol
  leaked the raw `Ty::Param("Self")` while `o.index(0)` yielded the protocol. An operator and its
  method spelling must not disagree.

And the termination class turned out to have a FOURTH member the first round missed:
`flatten_embed_methods` uses a `path` stack, which detects a cycle but does nothing about SHARING, so
it re-walked every shared subtree once per route. A 42-protocol `Pi: P(i+1), P(i+2)` DAG hung `check`
on the **declaration alone**, before any use — and hung identically pre-change, so it was never a
regression, merely never triggered. Fixed with the same visited set. **Lesson: "I fixed the
exponential walk" was a claim about the walkers I had touched, not about the class.** Grepping for
the *shape* (a recursive embed walk) rather than the symptom would have found all four at once.

### The lesson: a widening's negative control is the whole test

The first version of the fix shipped green against every acceptance and was still wrong.
`"x" in b` type-checked on a `b: Bag[int]` where `protocol Bag[T]: Contains[T]`. Cause:
`resolve_ty_ro` resolves a bare name through `self.type_params` — which at a USE site is the *calling
function's* params, not the protocol's, so the embed arg `T` resolved to `Ty::Unknown` (permissive
everywhere) or, when the caller happened to have a same-named param, to the CALLER's `T`. Fixed by
`embed_arg_tys`, which resolves an embed's args against the owning protocol's own type params first.

Nothing in the acceptance set could have caught it — every acceptance wants a `Some`, and `Unknown`
is a `Some`. Only the paired rejection (`"x" in b` must fail while `3 in b` passes) distinguishes
"resolved correctly" from "resolved to Unknown". Same shape as the
`rule-fires-is-not-rule-is-right` rule, one direction over: a new **accept** must be proven against
its own premise too, not just observed to accept.

### Tests

Two claims, two tests, per the W7-21 lesson. `src/checker/tests.rs` — 14 `ok()`/`rejects()` tests in
the M22 block, each acceptance paired with the negative that proves the rule did not go permissive
(unembedded method still rejected, unrelated protocol param still rejected, arg invariance intact,
mismatched protocol operands still rejected, and the parameterized-embed item type). These are the
only tests that can prove a *rejection* is gone — `run_file`/`run_file_parallel` bypass the checker.
`tests/chz/spec/protocol_embed_test.chz` — 8 `test fn`s running the same programs end to end,
gated serial==M:N by `test_runner::chz_suite_passes_both_engines`.

---

### W7-23 — the interpolation fragment scanner is not quote- or depth-aware — **FIXED 2026-08-06**

Measured on the release binary at `c9c4e26d`, all three valid programs:

```
print("{d['a}}b']}")     → type error: unmatched '}' in string (use '}}' for a literal brace)
print("outer {'inner {n}'}") → type error: lex error: unterminated string literal
print("{ {1, 2}.len() }")    → type error: unexpected an indented block in expression
print("{ 1 + 2 }")           → type error: unexpected an indented block in expression
```

CPython renders all four (the last two verbatim, the first two modulo f-string prefixes).

**Cause.** `src/interpolation.rs` scanned the fragment with `for ic in chars { if ic == '}' { break } }`
— no state at all. A `}` inside a nested string literal, or inside `([{`, terminated the fragment and
the truncated text was then lexed as an expression, which is where the incoherent second-order errors
("unterminated string literal", "an indented block") came from. The padding case is a different miss
in the same function: the fragment is handed to `lexer::tokenize_at` as its own line, so leading
whitespace opens an INDENT token.

**Fix.** The scan now carries the same `in_str: Option<char>` + `depth: i32` state machine
`crate::fmtspec::split_spec` uses, and only treats `}` at `depth == 0` outside a string as the
terminator; `parse_expr_str` trims its source. `unterminated '{'` and the bare `unmatched '}'` are
unchanged.

**Adversarial review caught a REGRESSION in the first cut.** Tracking quotes/brackets across the
WHOLE fragment broke every format spec whose fill character is `'`, `(` or `)`: `"{x:'>5}"` printed
`''''7` before (CPython prints the same) and became `unterminated '{' in interpolated string` after —
the fill `'` opened a string that never closed, `(` swallowed the closing brace, `)` drove the depth
negative. The spec is **literal text**, so the scanner now stops expression-tracking at the top-level
`:` and counts only brace nesting past it (CPython's own rule). The ternary check that keeps
`split_spec` from splitting `if c: a else: b` is now `fmtspec::is_ternary_head`, shared by both
layers — the same fix as the bug itself, one level up: two layers reading the same text must not each
carry their own copy of where the expression ends.

**Two limits stay, and are now documented** (`docs/syntax.md` §10) — both shared with CPython < 3.12:
a fragment cannot nest the *same* quote style (`"{d["k"]}"` — the LEXER ends the string literal
first, before interpolation is ever reached), and a nested literal is a normal Chezzi string, so it
interpolates too and a literal brace inside it is still doubled (`'a}}b'` is the key `a}b`).

**Why it survived.** `split_spec` — called on the very next line of the same function, on the very
same text — has been quote-aware since it shipped, with a comment saying so. The bug is not that
nobody thought about quotes; it is that the invariant was implemented in the *inner* of two adjacent
layers and the outer one was assumed to have it. Generalizes: when two layers parse the same text
one after the other, a property proven for one says nothing about the other.

### Tests

`tests/chz/spec/interpolation_test.chz` — 6 `test fn`s running the real programs end to end, gated
serial==M:N by `test_runner::chz_suite_passes_both_engines`. `compiler::interp_tests::parse_interpolation_scanner_is_quote_and_depth_aware`
covers the chunk shapes plus both surviving error paths (a bare `}`, an unclosed `{`) — the two cases
`assert` cannot see.

---

### W7-24 — call-argument normalization never reached an interpolation fragment — **FIXED 2026-08-06**

Measured on the release binary at `ba901280`. Each pair is the SAME call, once outside a string and
once inside one:

| program | outside | inside `"{…}"` (before) | after |
|---|---|---|---|
| `fn f(a: int, b: int = 2)` → `f(1)` | `3` | `type error: 'f' expects 2 argument(s), got 1` | **`3`** |
| `fn sub(x: int, y: int)` → `sub(y=1, x=10)` | `9` | `type error: 'sub' expects 2 argument(s), got 0` | **`9`** |
| `struct P(x, y = 5)` → `P(x=1).y` | `5` | `type error` | **`5`** |
| `fn bump(self, n, m = 3)` → `c.bump(2)` | `6` | `type error` | **`6`** |
| `fn sum_all(...xs: int)` → `sum_all(1, 2, 3)` | `6` | `expects 1 argument(s), got 3` + `argument 1: expected List[int], found int` | **`6`** |

A call through a fn VALUE (`sv := sub` then `"{sv(y=1, x=10)}"`) worked throughout — it takes the
`KeywordTable` path, which is resolved at check time rather than by the desugar pass.

**Cause.** `ExprKind::Str(String)` stored the literal's raw text with interpolation parsing deferred.
`desugar::run` (`resolver/mod.rs:381`) — the pass that normalizes named args, defaults AND variadic
sweeping, all three inside one `normalize_call` — ran while every fragment was still text. Fragments
were then re-parsed AFTER it, separately, by each consumer: the checker's `check_interpolation` and
`scan_expr_for_pin`, the compiler's `compile_str`, and the compiler's three capture walkers — three
re-parses of the same literal per compile, each re-applying only `desugar::lower_carriers`. So the
checker received precisely the shape desugar's module header says it never will:

> "the checker and the VM consume the already-normalized AST — they only ever see `Call.named` empty
> and a fully positional `Call.args`."

That is why the diagnostics were not merely wrong but incoherent: `got 0` (the named args were never
converted, so nothing was positional) and `expected List[int], found int` (the variadic parameter's
synthesized list slot, never swept).

**Fix.** `ExprKind::Interp(Vec<Chunk>)` — produced by `desugar`, never by the parser. The walker
rewrites a brace-carrying `Str` into parsed chunks and then walks the fragments as ordinary children,
so normalization runs on them **inside the live scope stack**: a local that shadows a fn name still
suppresses the rewrite, exactly as outside a string (`interpolation_fragment_respects_local_shadowing`
+ `fragment_local_shadows_fn_name`). `ExprKind::Str` now means "brace-free — or a malformed
interpolation, left intact so the checker/compiler report it with their existing message and span".
All five consumers read chunks; `parse_interpolation`/`interp_exprs` remain for the un-desugared
fallback path. Three re-parses per interpolated literal are gone as a side effect.

**Why it survived.** The bug is one phase-ordering decision (`// interpolation parsing deferred`,
five words in an enum) whose consequence is invisible from every site that depends on it. **An
invariant a pass establishes only holds for what that pass can SEE** — text stored inside a node is
not part of the tree, so "the AST is normalized" was true of the AST and false of the program.
Generalizes to every deferred-parse in the front end: raw text in a node is a hole in every
tree-walking guarantee, and the holes only show up at the consumer, as errors that describe a shape
the consumer was built to believe impossible.

### Tests

`tests/chz/spec/interpolation_test.chz` — 7 `test fn`s, one per surface (free-fn default, named args
in both orders, struct-ctor default, method default, variadic sweep including the empty sweep, nested
defaulted calls) plus `fragment_local_shadows_fn_name`, the guard that the rewrite does not go
permissive. Gated serial==M:N by `test_runner::chz_suite_passes_both_engines`.
`src/checker/tests.rs` — 5 tests, each acceptance paired with its negative:
`interpolation_fragment_call_args_are_normalized`,
`interpolation_fragment_wrong_arity_still_rejected_with_real_count` (the accurate count, both
directions), `interpolation_fragment_unknown_named_arg_rejected_by_desugar` (the new path — the same
error, from the same pass, as outside a string), `interpolation_fragment_respects_local_shadowing`,
and `interpolation_fragment_checked_without_desugar` (the `Str` fallback still checks and still
rejects a malformed literal).

---

### W7-25 — a string nested in a container/struct/enum rendered raw, so different values printed identically — **FIXED 2026-08-06** (BREAKING output change)

Measured on the release binary at `5076ab6a`, against CPython 3.14:

| value | before | after | CPython |
|---|---|---|---|
| `["a", "b"]` | `[a, b]` | **`['a', 'b']`** | `['a', 'b']` |
| `["a, b"]` | `[a, b]` — **same text, different value** | **`['a, b']`** | `['a, b']` |
| `[""]` | `[]` — reads as an EMPTY list | **`['']`** | `['']` |
| `{"k": "v"}` | `{k: v}` | **`{'k': 'v'}`** | `{'k': 'v'}` |
| `S(name="hi", n=1)` | `S(name=hi, n=1)` | **`S(name='hi', n=1)`** | `S(name='hi', n=1)` |
| `recover: [1][5]` | `Err(index 5 out of bounds (len 1))` | **`Err('index 5 out of bounds (len 1)')`** | — |

`str(a) == str(b)` was therefore **true while `a == b` was false**. Two consequences beyond the
cosmetic one: a one-element list of a comma-bearing string is indistinguishable from a two-element
list (it reads as a `split` bug that does not exist), and `[""]` prints exactly like `[]`.

**Fix.** `crate::slice::str_repr` — beside the existing `bytes_repr`/`bytearray_repr`, same escape
family, cross-checked value-by-value against CPython 3.14 (`'` normally, `"` when the string holds a
`'` and no `"`; `\\`, `\n`, `\t`, `\r`, the chosen quote, and ASCII control chars as `\xHH`;
non-ASCII literal, as in Python 3). Applied by a new `Vm::stringify_nested_into` at the six NESTING
sites and nowhere else: `stringify_seq_into` elements (list / tuple / **enum payload**), map key,
map value, set element, struct field, newtype inner.

**The one site deliberately excluded** is a `str(self)` display hook's RESULT (the three arms that
re-enter `stringify_into` at the SAME depth, for struct / enum / newtype). That string is the
object's own rendering, not a value nested inside it — quoting it would turn `[Tag(7)]` into
`['<7>']`. Fenced by `display_hook_output_is_never_quoted`. This is also why the rule could not be
implemented as "quote whenever `depth > 0`": the hook path preserves depth on purpose.

**Why it survived — the detector encoded the bug.** The CPython differential oracle
(`src/difftest/`) exists precisely to catch a Chezzi-vs-Python divergence, and its shim defined:

```python
def _chz_repr(v):
    return v if isinstance(v, str) else _chz_str(v)   # ← the divergence, written INTO the oracle
```

So the one tool built to find this could never report it. Eight difftest suites went red the moment
the implementation was fixed, and the arm is now literally `repr(v)` — the oracle proves nested
rendering EQUAL rather than absorbing a difference. Same family as
`lossy-decode-blinds-a-comparison-oracle`, one step earlier in the pipeline: **a detector written to
mirror the implementation is blind to bugs in what it mirrors** — when a shim absorbs a
"by-design difference", the design claim needs its own evidence, because the shim will never supply
it.

**Adversarial review caught the fix being HALF-APPLIED, twice.**
1. **A second and third renderer were untouched.** `display_guarded` (the `&self` structural form)
   and `display_wire` (the wire form, which is how a `Shared`/`RwShared`/`Atomic` payload renders)
   kept printing nested strings bare, so `print(Shared([" ", "a, b", ""]))` gave
   `Shared([ , a, b, ])` — three elements looking like four — while `print(s.get())` on the SAME box
   gave `[' ', 'a, b', '']`. The invariant this row exists to establish (`str(a) != str(b)` whenever
   `a != b`) was false on every wrapper-box path. Both now quote: `display_guarded` by `depth > 0`
   (it is `&self`, so it has no display-hook path that preserves depth), `display_wire`
   unconditionally (every one of its callers renders a nested position). Error/debug text changes
   with them, and that is the consistent answer — CPython's exceptions show `repr` too.
2. **Non-printable non-ASCII was still raw**, which is the SAME ambiguity one alphabet over:
   `["\u{a0}", " "]` printed as two identical-looking elements, and `["\u{200b}", ""]` hid a
   zero-width space exactly the way `[""]` used to print as `[]`. `str_repr` now escapes by
   printability (Rust's Unicode tables via `char::escape_debug`) at CPython's widths —
   `\xHH`/`\uXXXX`/`\UXXXXXXXX`, lowercase hex, verified value-by-value against CPython 3.14.
   **One deliberate residual:** Rust also treats grapheme-extend characters as non-printable, so a
   combining mark escapes here and prints literally in CPython. Escaping is the unambiguous
   direction, and a Unicode-category dependency for one category is not worth it.

*Both are the same meta-finding as W7-22's: a fix applied to SOME arms of an N-way set. The N here
was "the renderers" (three) and "the ambiguous characters" (two alphabets), and in both cases the
full green suite plus a fresh doc comment claimed the job was done.*

**Sweep.** 14 `examples/*.expected` regenerated (mechanically, by diffing each example's real
output; the remaining example diffs are concurrency line-order, which those tests compare sorted),
15 Rust expectations across `vm/tests.rs`, `vm/parity_tests.rs`, `vm/gc_tests.rs`, and 8 assertions
in `tests/chz`. Unchanged by design: `Op::ToStrFmt`'s top-level `FmtArg::Str` path, `json`
encoding, and `assert` messages. (`display`/`display_wire` were on this list until adversarial review
showed they are reachable from `print` — see above.)

### Tests

`tests/chz/spec/repr_test.chz` — 8 `test fn`s: the motivating ambiguity (`str(a) != str(b)` whenever
`a != b`), the empty-string element, containers, the bare-string non-case, quote choice + escapes,
struct fields, enum payload, and the display-hook exclusion. Gated serial==M:N by
`test_runner::chz_suite_passes_both_engines`. `slice::tests::str_repr_python_style` covers the
renderer directly against CPython's `repr` output. The strongest fence is the difftest suite itself:
with the shim arm now equal to `repr`, every fuzzed program compares Chezzi's nested rendering to
CPython's own.

### W7-26 — `--max-heap` never counted an `Executor`'s EAGER half, which is the only half M:N uses — **FIXED 2026-08-06**

> Found by adversarial review of the `W6-10r` fix, reproduced independently before filing, and the
> premise **re-derived on the release binary before any edit** (313 MB, unchanged by the four commits
> that had landed since the filing).
>
> ```chezzi
> import std.concurrency
>
> test fn execres():
>     blob := "".join(parts)          # ~1 MB, built once
>     ex := Executor()
>     for i in range(300):
>         ex.submit(fn() -> str: blob)
>     ex.shutdown()
>     assert true
> ```
> `chezzi test --max-heap=8000000 ex_test.chz` → **PASS, rc=0, peak RSS 313 MB → OVER-MEMORY, rc=1,
> peak 11 MB.** The same program at a 4 GB cap and with no cap still PASSES.
>
> **Mechanism.** `ExecutorCore` has TWO payload halves. `inner: Mutex<ExecState>` is the `--serial`
> queue and was the only one `Heap::live_bytes` read. On the default M:N engine `submit` runs EAGERLY
> (matching Python's `ThreadPoolExecutor`), so `inner` stays empty forever and every finished job's
> result lands in `eager: Mutex<EagerState>` as
> `TaskOutcome::Done(WorkerResult { value: WireValue, out, stderr })` — off-heap wire bytes plus two
> buffered-output `Vec<u8>`s. The half the cap read was exactly the half the default engine does not
> use.
>
> **The fix, in two halves — and the second one is the lesson.**
>
> 1. **Accounting.** `EagerState` gains the `(bytes, dirty)` summary `ChanState`/`ExecState` already
>    carry, maintained by `finish` (`core::outcome_summary` — `wire_summary` of a `Done`'s value plus
>    every variant's two output buffers) and reset by `take_slots`, over a slot vector made PRIVATE
>    for the same "no site can forget" reason as `ChanState::queue`. `live_bytes`'s `Obj::Executor`
>    arm now sums BOTH halves (locks taken sequentially, keeping `dispatch_eager_job`'s fixed
>    `inner → eager` order), and the nested-core recursion reaches cores inside a result.
>    The charge is **UNCONDITIONAL**, unlike the `mem_cap != 0` gates on `to_wire_crossable`'s pacing
>    charge and `live_bytes`'s `deep` walk. Those fire per-store / per-sweep; this fires once per
>    finished job, beside a thread handoff and a condvar notify, right after that job's own
>    `O(payload)` `to_wire`. Gating it would buy nothing and would make `live_bytes` mean two
>    different things depending on a flag — the shape that let `W6-10r`'s cap-off hole survive. Both
>    ancestors keep accounting live and the *limit* separate (Go's `runtime.MemStats.HeapAlloc` vs
>    `GOMEMLIMIT`; CPython's `gc`/`sys.getsizeof` vs `resource.setrlimit`).
> 2. **Sampling — without which the accounting is worthless, measured.** With (1) alone the repro
>    tripped at **180 MB, not 11 MB**: `over_cap` is assigned only in `sweep()`, `sweep()` runs only
>    when `should_collect()` fires, and a `submit` loop grows the parent's heap barely at all — so
>    hundreds of megabytes of results piled up between samples. `EagerState::take_charge` (the
>    GROWTH in `bytes` since it was last read — a delta, because the pacing counter is monotonic and
>    reset per sweep) is charged against the submitting heap's `charge_wire_bytes` at each `submit`
>    under a live cap. This is the W6-10 review lesson repeating one wave later: **the byte that is
>    counted but never looked at is the same as the byte that was never counted.**
>
> **Rooting is unchanged — and now FENCED rather than reasoned about.** A worker result crosses by
> value with no parent-heap `GcRef` (B3.2), which is why `Heap::children` still has no `eager` arm.
> That is an enforced invariant, not luck (`ensure_crossable` rejects a `Handle` on the way out), so
> `outcome_summary` carries `debug_assert!(!w.has_handle())` naming the consequence: if it ever
> fires, `children` needs the arm `live_bytes` just gained.
>
### W7-26r — nothing observed `--max-heap` while the parent was blocked in a join — **FIXED 2026-08-06**

> Filed as `W7-26`'s residual. Both halves are closed here — the join backlog and the pool-queue
> sibling — and **the filed premise had to be rebuilt first**: the original repro was return-value
> based, and `W7-27` (landed hours earlier) stopped retaining those. Every repro below is buffered
> OUTPUT instead, which is retained by contract (W7-5c flushes it at the task's slot).
>
> ```chezzi
> fn noisy():                     # builds its own ~1 MB and PRINTS it; captures NOTHING
>     …
> test fn exjoin():
>     ex := Executor()
>     for i in range(300): ex.submit(noisy)
>     ex.shutdown()               # the backlog accumulates HERE, where nothing samples
>
> test fn nursjoin():
>     parallel:
>         for i in range(300): spawn noisy()
> ```
> `--max-heap=8000000` on the release binary: **executor PASS at 622 MB, nursery PASS at 733 MB.**
> `over_cap` is assigned only in `sweep()`, `sweep()` runs only at the parent's own instruction
> boundary, and a parent inside a join reaches none. The nursery half was worse than a sampling
> gap — its outcome slots live in `SchedCore::slots`, outside every `Heap`, so `live_bytes` could
> not have counted them even if something had looked.
>
> **The ancestors decide the design, measured 2026-08-06 rather than reasoned about:**
>
> | | who observes the limit | while the joining thread is blocked |
> |---|---|---|
> | CPython 3.14.6, `ThreadPoolExecutor` + `RLIMIT_AS` 300 MB, 500 × 1 MB retained | the kernel, at each allocation, **in the allocating worker thread** | `MemoryError` in the worker at job **57/500** while `main` sat in `ex.shutdown()`; peak 230 MB |
> | Go 1.26, `GOMEMLIMIT=32MiB`, `GOGC=off`, 500 goroutines × 1 MB retained, `wg.Wait()` | the runtime GC pacer, driven by **allocating goroutines** | **7 GC cycles ran while `main` was blocked**; soft limit → thrash, no abort |
>
> Neither ancestor asks the blocked consumer. Neither does Chezzi now: `core::halt_over_backlog` is
> called by the two PRODUCERS — `dispatch_eager_job`'s pool closure and `MnSched::finish` — and when
> the join's retained backlog alone exceeds the whole cap, the finishing thread replaces its own
> `Done`/`Cancelled` outcome with a hard-halt over-memory `Fault` and trips its core/scope cancel so
> siblings stop feeding it. Everything downstream is inherited: `reduce_task_slots`'
> `Exit > hard-halt > ordinary` precedence propagates it, the `is_over_memory` marker keeps `recover:`
> from catching it, and the runner buckets it `OVER-MEMORY`. Only `Done`/`Cancelled` are replaced —
> demoting an `Exit` would lose an `os.exit`, and an existing `Fault` already halts.
> `MnSched::finish` now RETURNS whether the stored outcome aborts, because a caller-side `matches!`
> on the outcome it handed in would miss the conversion and leave the scope's parked siblings unwoken.
>
> **It cannot false-positive**: the trip needs the retained backlog BY ITSELF to exceed the entire
> cap, and those bytes are held until the join reduces the slots. Nothing estimates, nothing samples a
> heap mid-native-call, and nothing sweeps where values are unrooted — which is what ruled out the
> tempting alternative of polling `live_bytes()` from inside the join: it counts not-yet-swept
> garbage, so it can fault a healthy program (the `parked-is-not-stuck` wrong-verdict class).
>
> Result: **both PASS → OVER-MEMORY**, executor peak 622 → 65 MB. Controls still PASS: the same
> programs at a 4 GB cap, and 300 tiny spawns under the 8 MB cap.
>
> ### The sibling — a job dispatched but not started is owned by no heap — **FIXED with it**
>
> `prepare_eager_job` rebuilds each submitted closure into its OWN worker `Vm` at submit time, so a
> queue deeper than the pool is N fully-built worker heaps parked in `vm/pool.rs`'s global FIFO: each
> one comfortably under a per-heap `--max-heap`, their sum charged to nobody. Measured: 300 slow jobs
> each capturing ~1 MB → **PASS, rc=0, peak RSS 666 MB** against an 8 MB cap.
>
> A per-heap cap needs an owner, and the submitter is it — the work is its own and reachable only
> through its executor handle. `ExecutorCore::pending` carries the bytes (`Heap::live_bytes` of the
> freshly built worker, measured under a live cap only), added at dispatch and removed the instant a
> pool thread picks the job up, so the queued charge and the running heap's own never overlap.
> `live_bytes`'s `Obj::Executor` arm adds it.
>
> **And — the third time in this family — the accounting alone changed nothing: still PASS at 666 MB.**
> A loop submitting slow jobs finishes none of them, so `take_charge` stays 0 and the parent, which
> allocates almost nothing per submit, never sweeps. The same bytes therefore also pace the
> submitter's sweeps, one line at the same site. **PASS at 666 MB → OVER-MEMORY, rc=1, 395 MB.**
>
> ### Adversarial review filed two criticals against the sibling charge, and BOTH were real
>
> Both prosecutors found the same pair independently, each with a measured repro, and both were
> reproduced on the release binary before fixing. Both came from ONE line — `pending` was first
> measured with `Heap::live_bytes()`, taken while `core.inner` was held:
>
> 1. **Self-deadlock, and the comment 20 lines above named the hazard.** `live_bytes`'s
>    `Obj::Executor` arm re-takes `core.inner`, which the submit path holds across the dispatch;
>    `std::sync::Mutex` is not reentrant. A job capturing its own executor — the exact
>    `ex.submit(fn(): ex.…)` shape that comment exists for — **hung, rc=124**, under a cap and only
>    under a cap. (My own first check called it PASS: the shell pipeline made `$?` report `head`'s
>    exit status, not `chezzi`'s. Verify a hang with the exit code of the program, not of a pipe.)
> 2. **False OVER-MEMORY from aliasing.** `live_bytes` is a REACHABILITY walk, so it charged every
>    `Arc`-shared core the captured closure could reach — bytes the submitter's own `live_bytes`
>    already counts — once per queued job. 60 jobs capturing ONE ~1 MB `Shared` reported ~60 MB and
>    tripped a 20 MB cap while the true peak was 3.8 MB. The row's own "cannot false-positive" claim
>    was false as written, because the argument was about the backlog and the sibling charge is a
>    different quantity.
>
> Fix for both: `Heap::own_bytes` — the same walk with the shared-core arms skipped, so it counts only
> what the submit actually deep-copied, and takes no core lock at all. The measurement also moved off
> the `inner` lock (belt to that braces). Both are pinned by controls inside
> `over_memory_counts_jobs_queued_but_not_started`, and the aliasing one is mutation-verified:
> restoring `live_bytes` there turns it red.
>
> ### What is still open
>
> - ~~The general `W6-10s` residual (a): a heap that grows for reasons OTHER than a join backlog while
>   its fiber is inside one native call.~~ **CLOSED 2026-08-07 by `W7-29` (`:7279`)** — it wanted no
>   watchdog after all. `Vm::start_task` samples before dispatch, with the operands rooted on the
>   operand stack; the filed claim that the window had "no safe sample point" was simply wrong.
> - ~~**Per-heap containment is not per-process containment.**~~ The nursery verdict is right, but its
>   peak RSS is not contained: reduction-count preemption interleaves all N tasks, so N partial
>   payloads are live before any task finishes, and each individual heap stays under the cap.
>   **CLOSED 2026-08-07 as BY DESIGN — decided by measuring the ancestors, not by argument, and the
>   decision rule was fixed in advance so the result could not be rationalised:** *if BOTH owning
>   ancestors abort on N-concurrent-workers-each-under-the-limit, this is a drift and gets a
>   process-wide account; if either does not, the per-heap contract stands.* Measured 2026-08-07 on
>   the same shape (N workers, each payload comfortably under the limit, the sum well over):
>
>   | runtime | limit | result |
>   |---|---|---|
>   | CPython 3.14.6, 8 `ThreadPoolExecutor` jobs × 96 MiB | `RLIMIT_AS=512MiB` | **aborts** — `MemoryError` in the WORKER at job 3, surfaced in main via `future.result()`, exit 3 |
>   | Go 1.26.5, 8 goroutines × 16 MiB, main in `wg.Wait()` | `GOMEMLIMIT=32MiB GOGC=off` | **does NOT abort** — 5 GC cycles, `HeapAlloc=128MiB` against a 32 MiB limit, exit 0 |
>   | Chezzi, 8 tasks × 4 MB (also run as 500 `Obj`s each, so sweeps really run) | `--max-heap=8000000` | PASS at 99.4 / 69.4 MB |
>
>   **They disagree, so there is no ancestor verdict to copy** — and Chezzi's PASS already matches Go.
>   The split is structural, not incidental: `GOMEMLIMIT` is a SOFT target that makes the collector
>   work harder, while `RLIMIT_AS` is an OS limit on the whole address space — not a language-level
>   per-test guard at all. `--max-heap` is the soft, deterministic kind. Going process-wide would also
>   change the flag's CONTRACT (`future.md §1b`: "any single execution context whose live heap exceeds
>   `N` is aborted") and make a near-boundary verdict depend on task interleaving. Not built.
> - A cap verdict is **load-dependent**, and legitimately so: on a busy machine the pool falls behind,
>   more jobs really do queue, and the memory really is live. Both ancestors behave the same way. Do
>   not write a test that asserts a PASS which depends on the pool keeping up — size the payload so
>   the whole queue fits under the cap instead (this exact assertion failed in the full suite, and the
>   trip it reported was correct).

> ### Tests (`W7-26`)
>
> `vm::heap::live_bytes_counts_an_executors_eager_results` (results register cap-OFF and cap-ON, a
> core nested in a result is charged under a cap, `take_slots` returns to baseline),
> `vm::core::eager_charge_reports_growth_only` (delta semantics, and a post-drain result charging in
> full rather than being swallowed by a stale watermark), and
> `test_runner::over_memory_counts_an_executor_result_backlog` (the repro on BOTH engines — each
> exercises a different half — plus the generous-cap negative direction; **re-based onto buffered
> OUTPUT by `W7-27`**, since return values are no longer retained for it to count). All
> mutation-verified: deleting the charge in `finish` turns the first and third red.

> ### Tests (`W7-26r` + its sibling)
>
> `test_runner::over_memory_trips_while_the_parent_is_blocked_in_a_join` (both producers — the
> executor join and the `parallel:` nursery join — plus the generous-cap negative direction) and
> `test_runner::over_memory_counts_jobs_queued_but_not_started` (a 60-job queue the pool cannot keep
> up with, its generous-cap control, and a same-shape control whose whole queue fits under the cap).
> Each is mutation-verified against BOTH halves of its own fix independently: disabling the executor
> producer reds the `exjoin` arm and disabling `MnSched::finish`'s reds the `nursjoin` one; dropping
> either the `pending` accounting or its pacing charge reds the sibling test.
>
> The sibling test uses a `sleep_ms` body rather than a busy loop to hold its pool threads: an eager
> job runs on a plain `Vm`, so the wait blocks that thread just as effectively at no CPU cost — the
> busy-loop version loaded the machine enough to flake the suite's wall-clock timing tests.

### W7-27 — an `Executor` job's return value was retained though nothing can read it (~8× the ancestor) — **FIXED 2026-08-06**

> Also found by adversarial review of the `W7-26` fix, and the sharper half of it: `W7-26` makes
> these bytes *visible* to the cap, this row is about the fact that they existed at all. Premise
> re-measured on the release binary before any edit.
>
> ```chezzi
> import std.concurrency
> fn mk() -> str: …                 # builds its own ~1 MB blob
> ex := Executor()
> for i in range(300):
>     ex.submit(mk)                 # `submit` returns nil — the result is unreachable
> ex.shutdown()
> ```
>
> Numbers below are **uncapped `chezzi run`** peak RSS. `W7-26`'s 313 MB for the same captured
> program is not a contradiction: that was `chezzi test --max-heap=8000000`, a ~15× different
> harness (the runner caps, sweeps and reports; measured post-fix at 28 MB vs 423 MB for the same
> program) — always compare within one harness.
>
> | 300 jobs, ~1 MB each, results discarded | before | after | CPython 3.14.6 `ThreadPoolExecutor` |
> |---|---|---|---|
> | job builds its own (`ex.submit(mk)`) | **339 MB** | **45 MB** | **42 MB** |
> | blob captured (`ex.submit(fn() -> str: blob)`) | **666 MB** | **410 MB** | **17 MB** |
>
> (Peak RSS, no cap involved. CPython measured with `resource.getrusage(...).ru_maxrss`, futures
> discarded exactly as Chezzi discards them — `for i in range(300): ex.submit(mk)`, no list kept.)
>
> **Nothing can read the retained value.** `Executor.submit` returns nil (Chezzi has no futures),
> `WorkerResult.value` is `#[allow(dead_code)]`, and `Vm::reduce_task_slots` reads only
> `out`/`stderr`. The M:N NURSERY path already stored `value: WireValue::Nil` for exactly this
> reason — the eager executor path was the one place that still kept it, for the executor's whole
> lifetime.
>
> **Fix.** One line in `ReadyWorker::run_outcome` (`vm/mod.rs`): the `Done` outcome stores
> `WireValue::Nil` and the crossed value is dropped. `to_wire_at` + `ensure_crossable` still run —
> **the crossing is the FAULT contract, not the storage**: `to_wire_at` is the fallible half, so a
> return value that cannot cross (a generator closing a reference cycle, a depth/size cap) still
> faults at the submit site with the task's real span, independent of whether anyone keeps the wire
> form. (A *plain* returned generator is not that case — B3.3's Option B wires it to an inert `Nil`
> that faults only when reached, verified post-fix: `ex.submit(g)` with `fn g() -> Iterator[int]`
> runs clean, rc=0.) `W7-26`'s accounting is untouched and still needed: `out`/`stderr` are
> unbounded on their own, and the value arm of `outcome_summary` / `EagerState::values` now costs a
> `Nil` match — kept, not deleted, so the walk does not have to be re-derived if a result is ever
> stored again.
>
> **Residual (filed under `W7-26r`, and FIXED there the same day).** The CAPTURED variant is still
> 410 MB of RSS: each `submit` builds its OWN ~1 MB copy of the capture into a `ReadyWorker` that then
> queues in the process-global pool, owned by no heap and freed only when the job runs. CPython holds
> 17 MB there because a captured `str` is shared by reference — which the by-value airlock (B3.2)
> cannot do, so the RSS half is an isolation-model cost, not this bug. What `W7-26r` closed is the
> ACCOUNTING half: those queued copies are now charged to the submitter (`ExecutorCore::pending`), so
> `--max-heap` sees them instead of passing a 666 MB program at an 8 MB cap.
>
> ### Tests
>
> `test_runner::executor_results_are_not_retained` — the pre-fix `W7-26` program (300 jobs returning
> a captured ~1 MB blob) must now **PASS** the same 8 MB cap. The cap is the in-tree PROXY for the
> RSS claim: with `W7-26`'s accounting live, a retained backlog is exactly what `--max-heap` sees,
> which is why the identical program was `OVER-MEMORY` before. M:N only — `--max-heap` is an M:N cap,
> and `--serial` trips on its queued task closures instead. Plus the re-based
> `test_runner::over_memory_counts_an_executor_result_backlog` above. Both mutation-verified:
> restoring `value` turns the first red; deleting the charge in `EagerState::finish` turns the
> second's M:N arm red.

### W7-28 — `--max-heap` counted EVENTS, not bytes, so three shapes rode 30–77× past the cap — **FIXED 2026-08-07**

> Closes `W6-10s` residual (b) — the inline-scalar escape documented since `future.md §1b` shipped —
> and two siblings that were never filed, both found while fixing it. Premise re-derived on the
> release binary before any edit (`filed-residual-premise-goes-stale`); the filed number was an
> UNDERSTATEMENT.
>
> **The shape, and it is the most ordinary runaway loop in the language:**
>
> ```chezzi
> test fn grow():
>     xs: List[int] = []
>     i := 0
>     while i < 80000000:
>         xs.push(i)          # 8 B per iteration, ZERO allocations
>         i = i + 1
> ```
> `chezzi test --max-heap=8000000` → **PASS, rc=0, peak RSS 617.8 MB — 77× the cap.** Filed as "32×";
> there is in fact no ceiling at all, only patience.
>
> **Root cause: every trigger counted an EVENT, and each event class has a shape that adds unbounded
> bytes without raising it.** `over_cap` is assigned only in `Heap::sweep()`, and `sweep()` runs only
> when `Heap::should_collect()` fires. Its terms were an `Obj` COUNT (`since_gc >= next_gc`,
> `next_gc = (live*2).max(256)`) and charged off-heap WIRE bytes. Measured, all three PASS at
> `--max-heap=8000000`:
>
> | shape | what it moves | measured |
> |---|---|---|
> | `xs.push(i)` × 80 M | nothing — appends into the `Vec` behind an existing `Obj::List` | PASS at **617.8 MB (77×)** |
> | `big.extend(chunk)` × 150 | nothing, and only ~1200 instructions total | PASS at **~240 MB (30×)** |
> | `s = s + s` × 22 | `since_gc` = 22, under the 256 floor | PASS at **127.7 MB** |
> | `"x".repeat(20000000)` | `since_gc` = 1 | PASS |
>
> `Map[int,int]` (199.5 MB) and `Set[int]` (185.5 MB) fail open exactly like `List`. The **accounting
> was already correct** — `bytes_in` charges `Vec::capacity() * size_of::<Value>()` — so all four are
> pure OBSERVATION bugs, invisible to every behavioural assertion in the suite.
>
> **The first fix was wrong, and adversarial review is what proved it.** Round 1 added an INSTRUCTION
> TICK to `should_collect` (cap-gated, sampling every `cap/8` instructions) on the premise that "an
> instruction can grow the heap by at most O(1) bytes". `extend` refutes it: one instruction appends N
> values. The tick shipped green — full suite, both engines, clippy clean, the `push` repro fixed —
> and still let 240 MB past an 8 MB cap. **A proxy that is *nearly* proportional to bytes is not a
> byte counter, and the gap is exactly where the bug lives.**
>
> **Fix — charge BYTES at the three funnels that own byte growth.** Every byte a heap gains arrives
> one of exactly three ways, and each has a single door:
>
> | how bytes arrive | funnel | charge |
> |---|---|---|
> | a NEW object | `Heap::alloc` — the only constructor | the object's shallow backing |
> | growth IN PLACE | `Heap::get_mut` — the sole `&mut Obj` accessor | deferred delta (below) |
> | an off-heap wire payload | `Vm::to_wire_crossable` | unchanged, already charged (`W6-10`) |
>
> `get_mut` HANDS OUT the `&mut`, so the "after" size does not exist when it returns. It therefore
> arms `pending_mut: Cell<Option<(GcRef, usize)>>` with the object and its size now; the next settle —
> the next `get_mut`, or `should_collect`, which runs every instruction — re-measures the same object
> and charges the difference. A shrink charges 0 (monotonic, like the wire counter). `sweep()` clears
> the record, which is what makes it sound: sweep is the only thing that frees a slot, so it is the
> only thing that could leave the record aimed at a slot `alloc` then re-hands to a different object.
> `set_mem_cap` disarms too, so the soundness is local to `heap.rs` rather than an argument about all
> 65 callers.
>
> **Why the funnels and not the growth sites.** `push`/`insert`/`add`/`extend`/the map index-store/
> `bytearray.push`/… is an OPEN N-way set, and charging some arms of it is the mistake this repo has
> already been bitten by twice (`W7-22` — a fix applied to 8 of 14 arms, under a doc comment asserting
> it was complete, with the full suite green). `get_mut` was verified as the sole door: no
> `.obj.as_mut()` and no `slots[..]` access exists anywhere outside `heap.rs`. Same forget-proof
> argument that put the wire charge in `to_wire_crossable`.
>
> **One sizing table.** The per-`Obj` byte arms were lifted out of `bytes_in` into
> `obj_bytes_shallow`, now read by `bytes_in`, `alloc` and the settle — so a new `Obj` variant is
> sized once instead of drifting between three copies. **Core arms score 0 and take NO lock**, on
> purpose: reaching `core.inner`/`core.q` from inside `get_mut` would re-take a lock the caller may
> hold, which is the non-reentrant-`Mutex` self-deadlock (`hang, rc=124, under a cap only`) that
> `Heap::own_bytes` already exists for (see `W7-26r`).
>
> **Measured on the release binary, `--max-heap=8000000`:**
>
> | shape | before | after | RSS after |
> |---|---|---|---|
> | `push` × 2 M / 20 M / 80 M | PASS at 22.7 / 160 / **617.8 MB** | OVER-MEMORY, rc=1 | 11.6 / 11.5 / 11.4 MB |
> | `extend` × 150 → 30 M ints | PASS at **~240 MB** | OVER-MEMORY, rc=1 | 15.2 MB |
> | `s = s + s` × 22 | PASS at 127.7 MB | OVER-MEMORY, rc=1 | 25.2 MB |
> | `"x".repeat(20000000)` | PASS | OVER-MEMORY, rc=1 | 26.6 MB |
> | `Map` / `Set` × 2 M | PASS at 199.5 / 185.5 MB | OVER-MEMORY, rc=1 | 31.5 / 29.8 MB |
>
> Generous-cap (4 GB) controls all still PASS at their full footprint (127.5 / 45.7 / 199.6 / 185.6
> MB), so each trip is a MEASUREMENT and not "a growth loop is over the cap".
>
> **Cost, measured rather than claimed away.** Cap-off pays one `mem_cap != 0` load-and-branch in
> `alloc` and one in `get_mut`, and `should_collect` returns before touching any of it. `benches/run.chz`
> over 15 runs: everything inside noise except **`struct` ~+1%**, reproduced at 40 runs × 2 (+0.85%,
> +1.11%) — same sign three times on the alloc-heaviest bench, so it is recorded as real
> (`docs/benchmarks.md`). The `get_mut`-heaviest bench (`list`) moved −0.18% / −0.40%, i.e. not at all.
> A branchless form would have to run `obj_bytes_shallow` unconditionally, which is strictly worse
> cap-off, so the branch stays.
>
> ### Tests
>
> `vm::heap::wire_bytes_pace_a_sweep_only_under_a_cap` (re-based onto the renamed counter) and a new
> phase-per-funnel unit test; `test_runner::over_memory_trips_on_inline_scalar_growth` covers `push`,
> the map index-store, `extend` and `s = s + s`, both engines, each with its generous-cap control.
> **Mutation-verified INDEPENDENTLY per funnel** — disabling the `alloc` charge alone reds the
> `s = s + s` shape, disabling the `get_mut` settle alone reds the `push` shape — so neither charge
> can rot behind the other.
>
> **A test the fix flipped, and why it was not simply deleted.**
> `over_memory_trips_on_a_worker_payload_with_no_task_allocation` carried a control (`nospawn`)
> asserting PASS for a `push` loop over the cap, whose comment said verbatim that it must "fail
> LOUDLY" if a parent-side sampling fix ever landed. This is that fix. It was re-based to the new
> truth, and the write-up above it now records what was LOST: the test can no longer ATTRIBUTE its
> trip to the worker path, because a worker born over the cap implies the parent that built the
> payload was over it too, and the parent now samples. Review of the re-base also caught the re-base
> itself being confounded — it used `for i in range(200000)`, and `range()` materialises its own
> 1.6 MB `List[int]` before a single `push`, so the assertion passed with the loop body replaced by
> `pass`. It uses a `while` loop now.

### W7-29 — a task whose whole body is one native call was never sampled: the cap tracked who ran BYTECODE, not who held bytes — **FIXED 2026-08-07**

> Closes `W6-10s` residual (a), open since 2026-08-06 and filed there as having "no safe sample point".
> That sentence was wrong, and it is the sentence that kept the item shelved.
>
> **The whole bug in two programs.** Byte-identical payloads, same ~171 MB peak RSS, same 8 MB cap;
> only the task body differs:
>
> ```chezzi
> fn use(ys: List[str]) -> int:
>     return ys.len()
> fn build() -> List[str]:
>     xs: List[str] = []
>     i := 0
>     while i < 300000:
>         xs.push("<100 chars>")        # interned — the producer holds ~4.8 MB
>         i = i + 1
>     return xs
> fn inner():
>     xs := build()
>     parallel:                         # NESTED → the eager arm of register_task
>         spawn xs.len()                # ← all-native body
> test fn t():
>     parallel:
>         spawn inner()
> ```
> | task body | verdict |
> |---|---|
> | `spawn xs.len()` (native) | **PASS at 170.9 MB — 21× the cap** |
> | `spawn use(xs)` (bytecode, otherwise identical) | OVER-MEMORY |
>
> **Mechanism.** `over_cap` is assigned only in `Heap::sweep()`; `sweep()` runs only when
> `should_collect()` fires; and `should_collect()`'s only non-test caller is `run_until`'s dispatch
> loop, guarded by `while self.frames.len() > base_level`. `spawn xs.len()` compiles to
> `Op::SpawnMethod` → `PendingCall::Method` → `start_task` → `do_method_call` → `invoke_native`, and
> **pushes no frame** — so the loop body never runs once and the heap is never sampled. Not a
> mis-placed check: an unreachable one.
>
> **Why every earlier repro was masked, which is why this took three rounds to see.** `do_spawn` deep-
> clones the crossing payload into the SPAWNING heap first (`sched.rs:150`, a `to_wire`/`from_wire`
> round-trip on `&mut self`), and on the LAZY nursery path that copy stays rooted in `self.nurseries`
> until the join — so the PARENT is over the cap and trips, and the program looks guarded. Proved by a
> doubling test on distinct (un-interned) strings: `nospawn` flips at ~2.7 MB, `spawn` flips at
> ~6.5 MB — **2×**, which only a second full copy in the parent explains. The EAGER arm
> (`self.parallel && self.mn.is_some() && worker_count() >= 2`, a nested `parallel:`) consumes the task
> into `prepare_worker` immediately, so that copy is garbage within the same opcode and the mask
> lifts. The amplifier that makes the worker's copy exceed anything the producer holds is that
> `from_wire` does not re-intern: N aliases of one interned literal become N fresh `Obj::Str`.
>
> **Fix — `Vm::sample_mem_cap`, called from `Vm::start_task` under a live cap.** The `Method` arm
> already pushes receiver and args onto the operand stack before dispatch, and `Vm::collect` traces
> `self.stack` as its first root — so a direct `collect()` there is sound even with `self.frames`
> empty. Empty frames end `run_until`'s LOOP; they do not make a collect unsound. The `Call` arm parks
> `callee` + args on the stack across the sample and takes them back (`split_off` + `pop`), so
> `invoke_value` receives exactly what it did before; that arm matters because a `Callee::Builtin` /
> `Callee::Native` callee pushes no frame either. On a trip it returns
> `err(...).over_memory()` with the task's real span and NO `unwind_deferred` — there are no frames and
> no defers yet, and the marker alone buckets `OverMemory` and bypasses `recover:`.
>
> **TWO DOORS, and conflating them was this fix's own review finding.** The first cut DELETED
> `Heap::request_collect` as subsumed. It is not: `ReadyWorker::invoke` is a second task dispatcher
> that never routes through `start_task` (`prepare_worker_from_wire` → `dispatch_eager_job` →
> `run_outcome` → `invoke`), i.e. the eager-`Executor` job door. Both prosecutors filed it
> independently. Restored, and each door's mechanism now states what it owns:
>
> | door | mechanism | why that one |
> |---|---|---|
> | fiber (`spawn` / `parallel:`) | `sample_mem_cap` at `start_task` | samples BEFORE dispatch, so it covers a body that runs no bytecode — structurally out of the flag's reach |
> | eager `Executor` job | `request_collect` | those bodies are always closures, i.e. always bytecode, so the flag is always consumed at the job's first boundary |
>
> No witness exists for the class `request_collect` uniquely covers there (an `Arc`-core capture, where
> `obj_bytes_shallow` charges 0 and `since_gc` stays under 256 — the producer holds the same core, and
> `live_bytes` charges it per-heap by reachability, so the producer trips first). **"No witness built"
> is not "unreachable", and the flag costs three lines.**
>
> **Cost ceiling, stated not discovered later:** a full mark-sweep per task start. Cheap on M:N (a
> fresh per-worker heap), but O(live heap × tasks) on the serial engine where every fiber shares one
> heap. Reachable only through the in-process `run_tests_capped` helper, since `--max-heap` is refused
> with `--serial` at the CLI — the line to revisit if the cap is ever opened to serial runs.
> `benches/run.chz` is unmoved (cap-gated, and the bench sets no cap).
>
> ### Tests
>
> `test_runner::over_memory_trips_on_an_all_native_task_body` — the repro (`nat`, must be
> OVER-MEMORY), its **bytecode twin** (`bc`, otherwise identical, pinning that only the body differs),
> a **producer-only control** (`prod`, same payload with no `spawn`, must PASS — so a payload-size
> drift cannot silently take over the trip) and a generous-cap control. 0.10 s.
>
> **The test's first version could not fail on its own regression, and the fix for that is worth
> more than the test.** It asserted only `OVER-MEMORY nat`, which the parent's trip produces
> identically; its whole discriminating power rests on the eager arm being armed, i.e. on
> `worker_count() >= 2`. On a genuine single-core runner it was **green with the fix reverted**. Note
> `CHEZZI_THREADS=1` does NOT demonstrate this — that variable is read by `main::cmd_run`, not by the
> test helper — only `taskset -c 0` does, because the affinity mask is what
> `available_parallelism()` reads. The test now **controls the environment instead of asserting it**:
> it takes a new `TEST_WORKER_LOCK` and forces `set_worker_count(4)` behind an RAII guard that
> restores `0` on any panic, so it is deterministic on a 1-core box and a 96-core box alike. Verified
> under `taskset -c 0`: green with the fix, and red **on the real assertion** with the sample mutated
> out. (`pool()` is a `OnceLock` sized once, so a pre-existing 1-thread pool cannot be resized — it
> does not matter: both the eager gate and `mn_join`'s `nworkers` read `worker_count()` live, and pool
> helpers only accelerate a join the inline owner completes anyway.)

### W7-30 — the CPython differential oracle diffed a LOSSY DECODE, so a byte-only divergence reported `Match` — **FIXED 2026-08-07**
Found while re-deriving `W6-9r` item 1 (below) to decide whether it was worth fixing. It was not — those
compares die with `--serial` — but the same hole lives in the oracle `future.md §2b` names as serial's
**replacement**, which is permanent:

```
src/difftest/run.rs:250   let stdout = out_h.join().ok()?;                        // Vec<u8> off the pipe
src/difftest/run.rs:253   stdout: String::from_utf8_lossy(&stdout).into_owned(),  // …discarded one line later
src/difftest/run.rs:129   if chz.stdout == py.stdout { return Outcome::Match; }   // blind compare
```

`from_utf8_lossy` is not injective — `ff fe` and `fe ff` both decode to two U+FFFD — so a run where
Chezzi and CPython put DIFFERENT bytes on fd 1 was classified `Outcome::Match`. Both sides can emit
non-UTF-8 today: `io.stdout().write_bytes` has been byte-exact since **W6-9**, and CPython's
`sys.stdout.buffer.write` always was. Measured on this machine, all three agree on `ff fe` for
`\xff\xfe` — CPython, Go `os.Stdout.Write`, and `chezzi run` — so the *runtime* was never wrong; only
the detector was. Same class as **W6-9b** (the parity oracle) and the `from_utf8_lossy` seam in **W6-4**.

**Detector gap, not a live divergence:** `generate.rs` emits no byte-writing programs, so no seed in
the corpus could reach it. It matters because the corpus is meant to grow (`docs/bug-discovery.md`)
and because this oracle outlives the two-engine one.

**Fix.** `Capture.stdout`/`.stderr` are `Vec<u8>` — the bytes, not a decode. `classify`'s existing
`chz.stdout == py.stdout` needed no edit and is now byte-exact; keeping only the bytes makes the blind
compare **unrepresentable** rather than merely fixed. `Capture::stdout_text()`/`stderr_text()` return
`Cow<str>` (zero-alloc on the valid-UTF-8 path) for the three consumers that genuinely want text:
`is_host_panic` (a `"panicked at"` substring search), `allowlist::float_scientific_crossover` (a
*formatting* heuristic over numeric output), and the `describe` report. The allow-list matcher cannot
reintroduce the blindness, but **not for the reason first written here** — adversarial review caught
that claim: `classify` reaches `allowlist::check` from THREE arms (`run.rs:155`, `:172`, `:187`) and
only the stdout one has byte-compared first. The real guarantee is the matcher's own early return —
two byte-different stdouts that decode ALIKE hit `a == b` and yield `None`, from every arm.

`describe` also gained the line the fix makes necessary: on a `DivKind::Stdout` divergence whose two
sides decode ALIKE, it prints both byte strings in hex. Without it the report would show a `Divergence`
verdict over two identical-looking stdout blocks — a detector that is right and unreadable teaches the
same distrust as one that is wrong.

**Tests** (`src/difftest/run.rs`, RED before the change — `classify` returned `Match`):
`a_byte_only_divergence_is_not_a_match` builds the two captures directly (`ff fe` vs `fe ff`), asserts
the premise that their decodes really are equal, then asserts `Divergence { kind: Stdout }`; and
`a_byte_only_divergence_reports_the_raw_bytes` pins the report branch. Direct on `classify`, so no
process spawn and no dependence on the generator ever emitting such a program. The 27 pre-existing
difftest tests — which do spawn real `chezzi` + `python3` — stay green, so ordinary UTF-8 programs are
not newly reported as divergent.

**`W6-9r` item 1 is closed as WON'T FIX in the same change** — see its row in the index table.

### W7-31 — the float allow-list can downgrade a CHEZZI CRASH to a non-finding — **FIXED 2026-08-07**
Found by adversarial review of the `W7-30` branch, while checking a *different* claim. **Pre-existing
and unchanged by `W7-30`** — `allowlist::check` has had the same three call sites since the oracle was
built (`95fbbd5a`; `HEAD:src/difftest/run.rs:132`/`:149`/`:164`).

`classify` reaches `allowlist::check` from three arms, and the allow-list was written for exactly one
of them:

| arm | verdict being downgraded | is a float-formatting excuse relevant? |
|---|---|---|
| `run.rs:155` — both exit 0, stdout differs | `DivKind::Stdout` | **yes** — this is what the matcher is for |
| `run.rs:172` — chezzi exited NON-ZERO, python exited 0 | `DivKind::ChezziFault` | no |
| `run.rs:187` — chezzi exited 0, python non-zero | `DivKind::PythonFault` | no |

`float_scientific_crossover` looks only at the two stdouts and never at `code`. Trigger: Chezzi prints
`1e-05` and then **faults** (exit 1) while CPython prints `0.00001` and exits 0 — `a != b`,
`a_sci != b_sci`, both `both_numericish` → `Outcome::AllowListed`, and a Chezzi crash is reported as a
non-finding. A genuine *Rust host panic* is still safe (the `is_host_panic` early return at `:167`
precedes the matcher), so this silences ordinary runtime faults, not aborts.

**Filed fix vs. shipped fix — the premise decayed.** This row was filed prescribing a per-MATCHER gate:
`float_scientific_crossover` should require `chz.code == Some(0) && py.code == Some(0)`, because
`MATCHERS` is an extension point and a future entry may legitimately apply to a fault arm. Re-deriving
that premise on the release binary before implementing found it dead, so the shipped fix is a
**deletion**, not the prescribed gate. Same lesson as `W7-4b` / `filed-residual-premise-goes-stale`: a
filed residual's premise decays under later commits, and the price tag (here, "gate the matcher") is
only as good as the premise it was priced against.

`float_scientific_crossover`'s stated excuse was *"Rust's `{}` and CPython `repr` … switch to
scientific notation at different magnitudes."* That is false for Chezzi, measured on the release binary
against CPython at every crossover boundary:

```
value      1e16    1e15                 0.0001   0.00001   1234567890123456.0   1e100    1.5e-7
chezzi     1e+16   1000000000000000.0   0.0001   1e-05     1234567890123456.0   1e+100   1.5e-07
cpython    1e+16   1000000000000000.0   0.0001   1e-05     1234567890123456.0   1e+100   1.5e-07
```

Byte-identical throughout — not by luck: `vm::format_float` (`src/vm/mod.rs:4135-4140`) delegates to
`fmtspec::repr_float` (`src/fmtspec.rs:455`), which implements CPython's `repr()`/`str()` crossover
rule directly (scientific when the decimal exponent is `< -4` or `>= 16`). The entry described a
divergence that does not occur.

**Scope that finding precisely — it is about the CROSSOVER, not float formatting in general.** The
adversarial review of this very change fuzzed 20 000 random `f64` bit patterns against CPython 3.14.6
and found **6 mismatches**, all one shape: `repr_float`'s shortest-repr *digits* come from Rust's
formatter, which breaks an exact half-way tie **away from zero** where CPython breaks it **to even**
(`-887777373534812.25` → chezzi `-887777373534812.3`, CPython `-887777373534812.2`, and
`float(...2) == float(...3)`). That is a real divergence, filed as **`W7-32`** and fixed there. It is
not allow-list material — it is a bug, and this oracle should report it, which after the deletion it
now can. Two corrections this forces on the record: the original filing of this section cited
`vm/parity_tests.rs:1547 python_float_repr_str_parity` as "pinning" CPython parity, and it does not —
it is a serial==M:N golden against a **hardcoded literal**, so it could never have caught `W7-32`; and
a 7-value hand-picked boundary table is evidence about the boundaries it names and nothing more.

The deleted entry was also unfireable on any *generated* program by two independent mechanisms:
`generate.rs`'s `gen_float` restricts itself to short exact-ish decimals specifically to dodge the
crossover and always emits a literal; `float_lit` is shared byte-for-byte between the Chezzi and Python
emitters; and `Features::full()` has `floats: false` regardless. (All three of those
restrictions were lifted by **`W7-37`** 2026-08-07 — the generator now emits float arithmetic and
`full()` has `floats: true` — which does not revive the matcher: the premise was still dead.)

A second, independent defect confirmed deletion over gating: the matcher never checked the two numbers
were numerically **equal** — `1e-05` vs `0.00002`, a genuine arithmetic divergence, was also silently
downgraded — and its own comment promised *"the lengths are close — a conservative guard so it never
masks a real arithmetic divergence"* while no such check existed in the body.

**Fix.** `src/difftest/allowlist.rs`: deleted `float_scientific_crossover` + its `both_numericish`
helper, leaving `MATCHERS: &[Matcher] = &[]`. `check()` gained the floor `classify` should never have
needed a matcher to provide: `if chz.code != Some(0) || py.code != Some(0) { return None; }`, before the
loop — an allow-list excuses a *formatting* difference between two SUCCESSFUL runs, and a non-zero exit
on either side is never that. The `Matcher` type, `MATCHERS`, and `check` all stay (the extension point
the row wanted preserved); a future entry that genuinely needs a fault arm moves the floor down into
that matcher (and the others), it is never simply deleted. Module header rewritten to record why the
only entry died, so nobody re-adds the same excuse.

**Tests** (`src/difftest/run.rs` `mod tests`, RED before the change — both returned `AllowListed`
instead of `Divergence`): `a_chezzi_fault_next_to_a_float_reformat_is_not_allow_listed` pins the exact
W7-31 trigger (`chz` exit 1 printing `1e-05`, `py` exit 0 printing `0.00001`) on the `ChezziFault` arm;
`a_python_fault_next_to_a_float_reformat_is_not_allow_listed` pins the same shape on the `PythonFault`
arm. Both direct on `classify`, no process spawn. `cargo test --test difftest` (30 passed, the real
subprocess suite including `fuzz_full`/`fuzz_straight_line` over seeds 0..120) and the heavy ignored
sweep (`fuzz_full_heavy`, seeds 0..3000, 99 s) both stayed green after the deletion — no corpus seed
was relying on the downgrade.

### W7-32 — a float's shortest `repr` broke an exact tie AWAY FROM ZERO; CPython breaks it TO EVEN — **FIXED 2026-08-07**
**How it was found: by re-deriving `W7-31`'s premise.** `W7-31` deleted an allow-list entry that
excused "Rust `{}` and CPython `repr` switch to scientific notation at different magnitudes" — measured
false, the crossover is byte-identical. But the dead entry was pointing at roughly the right
*neighbourhood* for entirely the wrong *reason*: float formatting really did diverge, one layer down,
in the shortest-repr **digits**. An independent fuzz of 20 000 random `f64` bit patterns against
CPython 3.14.6 (adversarial review of the `W7-31` branch) found **6 mismatches, all one shape**.

```
$ cat f.chz                          $ cat f.py
print(771.5462036132812)             print(771.5462036132812)
print(1007730844620651.2)            print(1007730844620651.2)
print(-887777373534812.25)           print(-887777373534812.25)

$ chezzi run f.chz                   $ python3 f.py
771.5462036132813                    771.5462036132812
1007730844620651.3                   1007730844620651.2
-887777373534812.3                   -887777373534812.2
```

**It is not the lexer** — both sides parse the literal to the same `f64`:
`printf 'x := 771.5462036132812\nprint(x == 771.5462036132813)\n' | chezzi run /dev/stdin` → `true`,
and in CPython `float('771.5462036132812').hex() == float('771.5462036132813').hex()` (both
`0x1.81c5ea0000000p+9`). It is the *printer*.

**Root cause.** The exact values are `771.54620361328125`, `1007730844620651.25`,
`-887777373534812.25` — the decimal expansion terminates in an exact `5` one digit past the cut, so
the two candidate shortest reprs are **exactly equidistant**. Rust's *shortest* float formatter breaks
that tie **away from zero**; CPython's `repr` (David Gay's `_Py_dg_dtoa`) breaks it **to even**.
Confirmed directly against rustc, independent of Chezzi:

```rust
println!("{} | {:e}", 771.5462036132812f64, 771.5462036132812f64);
// => 771.5462036132813 | 7.715462036132813e2      (both Rust forms; CPython says …812)
```

**Scope — narrow, and that is what makes the fix cheap.** Only the two *shortest* branches of
`repr_float` were wrong. Rust's **fixed-precision** formatter (`{:.N}`) is exact and already rounds
half-to-even, so every `{:.Nf}`/`{:.Ne}`/`{:.N%}` format-spec path was already CPython-correct —
measured, both sides identical:

```
"{:.2f}"(0.125)=0.12  "{:.2f}"(0.135)=0.14  "{:.1f}"(2.5)=2.5
"{:.1f}"(0.25)=0.2    "{:.3e}"(106250.0)=1.062e+05  "{:.0f}"(0.5)=0  "{:.0f}"(1.5)=2
```

Blast radius of the bug (and of the fix): `repr_float` is the single source of truth for the bare
stringify path — `str(f)`, `print(f)`, `{f}` interpolation with no type char, `json.stringify`, and
the `None`-type-char spec arm (`fmtspec.rs:379`).

**Fix** (`src/fmtspec.rs`, `repr_float`). *Reuse Rust's own exact half-even formatter rather than
hand-rolling digit surgery over a 1074-digit expansion.* CPython's `repr` is "the nearest decimal with
the shortest round-tripping digit count, ties to even"; Rust's shortest gives that same digit count,
and `{:.N}`/`{:.Ne}` at that count is exactly "nearest, ties to even". So:

1. Count the significant digits `D` of Rust's `{:e}` mantissa. If its **last digit is even**, return
   unchanged — Rust and CPython can only disagree when away-from-zero landed on an odd digit. This is
   the fast path and it never re-formats.
2. Otherwise re-render at the same `D`: `{x:.*e}` with `D-1` (scientific branch) or `{x:.*}` with
   `D-1-exp` fractional digits (fixed branch).
3. Keep the re-render **only if it still round-trips** (`parse::<f64>()` compared by `to_bits`).

Step 3 is not belt-and-braces — it is load-bearing, and the fuzz proved it: at a **binade boundary**
the even candidate can fall outside the value's rounding interval. `2^-24` is exactly
`5.9604644775390625e-08`, an exact tie, but `5.960464477539062e-08` parses to a *different* float, so
the even candidate is not a legal repr and CPython keeps the odd `…063` too. A first cut of this fix
without step 3 was green on the whole test suite and on the brief's 5 400-value differential, and was
caught only by a 60 000-value tie-rich dyadic (`m/2^k`) fuzz — 31 regressions, all this one shape.
The `x.fract() == 0.0` branch is untouched (an integer-valued double is exact — no tie is possible)
and no `{:.N}` path is touched.

**Tests.**
- `src/fmtspec.rs::python_float_repr_and_e_spec` — the existing CPython-differential table grew seven
  rows: the three measured ties above (incl. a **negative**, since the rule is derived on the
  magnitude's digits), `2.9802322387695312e-08` (`2^-25`, a tie in the **scientific** branch, found by
  an exhaustive search of the two families that can produce one there), the binade-boundary
  non-adjustable tie `5.960464477539063e-08` (`2^-24`), and two near-ties that must NOT move —
  `5e-324` (odd last digit, and the decremented neighbour `4e-324` *does* round-trip but is farther)
  and `0.1`. Every expected string is a real `python3 -c "print(repr(...))"` run.
- `tests/chz/spec/conversions_test.chz::float_repr_breaks_an_exact_tie_to_even` — the same assertions
  in Chezzi (`str(...) == "..."`), so this observable-output change is pinned at the language level and
  gated serial==M:N by `test_runner::chz_suite_passes_both_engines`.
- End-to-end CPython differential, post-fix, **all identical**: the brief's two runs (400 and 5 000
  mixed-magnitude values), 100 000 uniform random `f64` **bit patterns**, 60 000 dyadic `m/2^k`
  (tie-rich), 20 000 `n/8` product chains + values straddling the `1e15`/`1e16` crossover, 20 000
  sci-branch tiny/huge, and all 8 391 powers of two with their immediate neighbours — 213 791 floats,
  zero diffs.

**Two record corrections this forces.** (a) `vm/parity_tests.rs::python_float_repr_str_parity` is a
serial==M:N golden against a **hardcoded literal**, not a CPython differential — it could never have
caught this, and `W7-31`'s original filing citing it as a CPython-parity pin is part of why W7-32
stayed invisible for so long. Its doc comment now says what it actually pins (assertions unchanged;
its values are unaffected by this fix). (b) There was no automated CPython float-repr differential at
all; the difftest generator's `gen_float` deliberately emits only short exact-ish decimals and
`Features::full()` has `floats: false`, so the oracle that *should* own this cannot currently reach it
— the table test + the chz test are the standing guard. **Closed by `W7-37` (2026-08-07):** that is
precisely the gap it fixes, and this row is the motivating example cited there — the oracle now emits
float arithmetic over both sides of the sci-notation crossover, with `floats: true` in `full()`.

### W7-33 — the CPython differential's `classify` never checked for a signal kill, and never checked for a host panic on the both-exit-0 arm — **FIXED 2026-08-07**

**Found by:** an audit of the whole CPython differential oracle (`src/difftest/run.rs::classify`)
for the "real bug reported as a non-finding" class — the same class `W7-31` had just been fixed
in, one call away. Two holes, same shape: the highest-value finding this oracle can produce (a
Rust-level host crash) was invisible on the arms that never looked for it.

**Hole 1 — a signal kill was never examined anywhere in `classify`.** `Capture.code == None`
means the child was killed by a signal (SIGSEGV / SIGABRT / a Rust stack overflow — which prints
`thread 'main' has overflowed its stack` and dies WITHOUT the `panicked at` marker `is_host_panic`
looks for). Before this fix:

| chz | py | before | after |
|---|---|---|---|
| `{stderr: b"\nthread 'main' has overflowed its stack\n", code: None}` | `{code: Some(1)}` | `BothError` (non-finding, `is_finding() == false`) | `HostPanic` |
| same `chz` | `{code: Some(0)}` | `Divergence{ChezziFault}` — and it passed through `allowlist::check` | `HostPanic` |

The twin oracle one directory over, **`panicfuzz::classify`** (`src/panicfuzz/run.rs:98-100`),
already had this exact rule (`cap.code.is_none()` → `Outcome::Crash`) — this was a divergence
between two sibling oracles built by the same team for the same purpose, not a novel design
question that needed re-deriving.

**Hole 2 — `is_host_panic` never ran on the both-exit-0 arm.** `classify`'s first arm compares
stdout bytes and returns `Match`/`Divergence` without ever consulting `is_host_panic`. A
**worker thread** panicking on stderr doesn't change the process's own exit code, so:

```
chz = { stdout: b"1\n", stderr: b"thread '<chezzi-worker>' panicked at src/vm/stream.rs:120:\n...", code: Some(0) }
py  = { stdout: b"1\n", stderr: b"",                                                              code: Some(0) }
   before: Outcome::Match        after: Outcome::HostPanic
```

Chezzi really does spawn threads that can panic independently of `main`: `src/vm/stream.rs:54`,
`src/native/request.rs` (six sites), `src/native/rand.rs:323`, `src/native/cffi.rs:2414`. Not
generator-reachable today (the difftest IR has no concurrency constructs), so this was defence
for the verdict logic rather than a live escape.

**Proximate cause.** `Capture.code`'s own doc comment read `// None => killed by signal /
timeout`. The "/ timeout" half was false — `run_sources` returns `Outcome::Timeout` the moment
`run_one` returns `None`, before a `Capture` is ever constructed, so a live `Capture` reaching
`classify` can only have `code: None` from a signal kill — and that wrong comment is plausibly
why nobody classified it.

**Fix** (`src/difftest/run.rs::classify` only — `allowlist.rs`, the generator, and `run_one` are
untouched). Both checks now run FIRST, unconditionally, before any arm-specific logic and before
any `allowlist::check` call: `is_host_panic(&chz.stderr_text())` then `chz.code.is_none()`, both
returning `HostPanic { chz }`. The redundant `is_host_panic` call that used to live inside the
`!chz_ok` arm is deleted (dead code once the top-of-function check subsumes it). The Python side
is deliberately NOT mirrored: a "Python host panic" isn't a thing CPython has, and
`py.code.is_none()` (CPython killed by a signal) is real but not a Chezzi bug — it already makes
`py_ok` false and falls through to the ordinary `PythonFault`/`BothError` arms. Note `PythonFault`
IS a finding (`is_finding()` is true for every `Divergence`), deliberately: a CPython crash on *our*
rendering usually means the EMITTER is wrong and is worth surfacing. What the Python side skips is
only the `HostPanic` promotion, which is reserved for a bug in our own runtime.

**One dormant over-fire vector, recorded not fixed** (found by the adversarial review of this
change): `is_host_panic` greps chezzi's stderr for the literal `panicked at`, and `panic(msg)`
(`src/vm/stmt.rs`) stores the caller's string verbatim into an uncaught fault's stderr. A
hand-written `.chz` that calls `panic("... panicked at ...")` would therefore be reported as a
`HostPanic`. Pre-existing — this change widens it from the `!chz_ok` arm to all three. NOT a live
escape: `generate.rs` never emits `panic()`, and `rand_str` caps string literals at 7 chars, too
short for the 11-char needle. If the generator ever grows a `panic()` construct, `is_host_panic`
must stop matching a bare substring of user-controlled text.
`describe`'s `HostPanic` arm (`src/difftest/mod.rs`) now prints `chezzi killed by a SIGNAL (code:
None)` when `chz.code.is_none()`, so an empty-stderr signal death doesn't render as a silent empty
block under a `HostPanic` verdict — same defect class `W7-30` fixed in the same function.

**Tests** (`src/difftest/run.rs`'s `#[cfg(test)] mod tests`, direct on `classify`, no process
spawn): `a_signal_killed_chezzi_is_a_host_panic`, `a_signal_killed_chezzi_is_a_finding_even_when_python_also_failed`
(asserts `is_finding()`, the property that matters), `a_worker_thread_panic_with_exit_zero_is_a_finding`,
plus two negative guards — `a_clean_chezzi_fault_is_still_an_ordinary_divergence` (an ordinary
runtime fault with no `panicked at` and a real exit code must stay `Divergence{ChezziFault}`, not
get promoted) and `a_both_ordinary_faults_stays_botherror`. All 3 positive tests were RED before
the fix (`Divergence{ChezziFault}`/`BothError`/`Match` respectively) and GREEN after; both
negative guards passed throughout. Full gate re-run post-fix: `cargo test --test difftest` (35
passed, 1 ignored), `cargo test --test difftest -- --ignored` (`fuzz_full_heavy`, seeds 0..3000,
94 s — no previously-passing seed became a finding), `cargo clippy --all-targets -- -D warnings`
clean.

### W7-34 — a child the CPython differential oracle could not even START was scored as "0 findings, exit 0" — **FIXED 2026-08-07**

**Found by:** auditing the whole oracle for the "real bug reported as a non-finding" class after
`W7-31`, the same audit that found `W7-33`. Measured trigger, moving the built `chezzi` binary
aside and running the release fuzzer with a bare `PATH`:

```
$ env PATH=/usr/bin:/bin ./target/release/difffuzz --seeds 0..50
done: 50 seeds, 0 finding(s) [(0, 50)]     exit code 0
```

**A green fuzz run over zero executed programs.** `difffuzz`'s `locate_chezzi()` falls back to the
bare name `"chezzi"` when no sibling binary exists next to the fuzzer (`src/bin/difffuzz.rs:141`,
inside `locate_chezzi` at `:133-142`),
so every seed's `Command::new("chezzi").spawn()` failed with `ENOENT`. `run_one` (`src/difftest/
run.rs`) collapsed FOUR distinct failure modes into a single `Option<Capture>::None`: the child
could not spawn (`.spawn().ok()?`), a stdio pipe wasn't there to `take()`, `try_wait()` itself
errored, and a genuine wall-clock timeout. `run_sources` mapped every `None` to
`Outcome::Timeout { which }`, and `Outcome::is_finding()` returns `false` for `Timeout` — so
"the harness never started a single child" and "every generated program ran and agreed" printed
the identical `0 finding(s), exit 0`. Same shape one level up: `write_file`'s staging failure
(an unwritable `TMPDIR`) returned `Outcome::BothError`, also a non-finding. Nothing anywhere
counted how many seeds actually executed. **The general lesson: a detector's failure to run must
not be representable as a clean result** — a 10 000-seed CI-passing fuzz run and a 10 000-seed
`ENOENT`-per-seed run were, before this fix, bit-for-bit the same report.

**Fix.** `run_one` now returns `Result<Capture, RunErr>` (`RunErr::TimedOut` /
`RunErr::CouldNotRun(String)`) instead of `Option<Capture>` — chosen over a bespoke 3-variant enum
because the two real dimensions here (an ordinary expected outcome vs. a harness-is-broken one)
map directly onto `Result`'s own vocabulary, and every existing call site already read as a
`match` over two arms. All three non-timeout `None` sites (`.spawn().ok()?`, both
`child.std{out,err}.take()?`, the `Err(_)` arm of `try_wait`) now build a `CouldNotRun` message
carrying the program name and the real `io::Error` text, e.g. `could not run "chezzi": No such
file or directory (os error 2)` — not just "failed". `Capture::code`'s "`None` means killed by a
signal, never a timeout" invariant (pinned by `W7-33`) is unchanged: a `RunErr` is returned before
any `Capture` is ever constructed, on every arm. `Outcome` gains `HarnessError(String)` — routed
from `run_one`'s `CouldNotRun` and from `write_file`'s staging failure (moved off `BothError`,
which is reserved for "both engines disagreed", not "the harness never wrote the source files").
`is_finding()` stays `false` for it — it is not a Chezzi bug — but the two callers now treat it as
FATAL rather than silently absorbing it: `tests/difftest.rs::fuzz_range` `panic!`s on the first
one instead of accumulating it into `findings` (3000 identical ENOENT messages help nobody), and
`src/bin/difffuzz.rs` prints it to stderr and exits **2** — distinct from the existing exit **1**
for "findings were found", so a caller can tell "the oracle broke" from "the oracle worked and
found bugs". `locate_chezzi`'s bare-name `PATH` fallback is left as-is: it is what turns a missing
build into `ENOENT` per seed, but with `HarnessError` that failure is now loud on the very first
seed instead of silent for all of them, so the fallback stopped being the dangerous part.

**Tests** (`src/difftest/run.rs`'s `#[cfg(test)] mod tests`): `a_missing_chezzi_binary_is_a_harness_error_not_a_timeout`
(a real `run_sources` call against a nonexistent `chezzi_bin`, asserting `HarnessError` naming the
path and the OS error — today this returns `Timeout { which: "chezzi" }`),
`a_harness_error_is_not_a_finding`, and `a_real_timeout_is_still_a_timeout_not_a_harness_error`
(guards the two positive tests against the two cases being silently collapsed — exercised at the
`run_one` level with `sleep 5` under a 50ms timeout rather than a real `chezzi` binary on an
infinite loop, for speed and to avoid flaking the unit suite). All three RED before the fix
(compile error: `Outcome::HarnessError`/`RunErr` did not exist), GREEN after. Full gate re-run:
`cargo test --test difftest` (38 passed, 1 ignored), `cargo clippy --all-targets -- -D warnings`
clean, plus the end-to-end proof the suite itself cannot give (moving the built `target/release/
chezzi` aside — it sits next to `difffuzz` and would otherwise be found as a sibling regardless of
`PATH` — then running with a bare `PATH`):

```
$ env PATH=/usr/bin:/bin ./target/release/difffuzz --seeds 0..50
harness error at seed 0: could not run "chezzi": No such file or directory (os error 2)
exit=2
```

### W7-35 — `panicfuzz` has the identical F1 bug `difftest` just fixed as `W7-34` — OPEN, filed not fixed

**Found by:** the adversarial review of the `W7-34` fix, per this project's own established pattern of
auditing the twin oracle whenever one of the pair gets a fix (`W7-33` is the precedent: `panicfuzz`
was the one with the RIGHT rule there, and `difftest` had to catch up).

`src/panicfuzz/run.rs` is a deliberate copy of `src/difftest/run.rs`'s subprocess machinery (its own
header says so: "This is a deliberate copy, not an import"), and it still has the bug `W7-34` fixed on
the `difftest` side. `run_one`'s `.spawn().ok()?` (`src/panicfuzz/run.rs:134`) returns `None` on a
child that could not even start (missing `chezzi_bin`, bad path, `ENOENT`) — the exact same case as an
ordinary wall-clock timeout, both of which are `None`. `run_input` (`:65-82`) passes that straight to
`classify` (`:90`), which maps `None` to `Outcome::Timeout` unconditionally — and `Outcome::is_finding`
is `false` for `Timeout`. So a `panicfuzz` run against a `chezzi` binary that cannot be spawned reports
every input as a clean, boring timeout instead of aborting — the crash-detector's own "did the
front-end crash" question was never asked for a single input, and nothing distinguishes that from a
sweep that genuinely found nothing.

**Mechanism, concretely:** same shape as `W7-34`'s repro — point `Config::chezzi_bin` at a path that
does not exist (or run with an unwritable `TMPDIR`, which hits the `write_file` staging arm at
`run_input`'s `:71-73`, itself mapped to `Outcome::Timeout` too) and every input in the sweep comes
back `Timeout`, indistinguishable from "the front-end is rock solid."

**Not implemented here** — out of scope for the `W7-34` fix pass per its own brief. `difftest` fixed
this with `run_one: Result<Capture, RunErr>` + `Outcome::HarnessError(String)` + the two callers
(`fuzz_range`, `difffuzz::main`) aborting instead of scoring it. The same shape applies to
`panicfuzz::run_one` / `Outcome` / `run_input`'s staging arm, and to `panicfuzz`'s own caller(s) (the
CLI fuzzer binary and any `#[test]` sweep) — but `src/panicfuzz/` is a hand-maintained sibling of
`src/difftest/`, not a shared module, so the fix needs its own pass rather than a shared helper.

### W7-36 — a Chezzi hang was thrown away without ever running Python, and a both-failed run's stdout divergence was invisible — **FIXED 2026-08-07**

**Found by:** the same "real bug reported as a non-finding" audit as `W7-30`–`W7-34`, this time on
the two remaining arms of `classify` / `run_sources` (`src/difftest/run.rs`) that discarded real
signal. Same family, two more findings.

**F3 — a Chezzi hang was thrown away, and Python was never even run.** `run_sources` returned
`Outcome::Timeout { which: "chezzi" }` (`is_finding() == false`) the instant the chezzi child
timed out, without running Python at all — so `run_sources(&cfg, "while true:\n    x := 0\n",
"print(0)\n", None)` reported nothing even though Python finishes the identical program in
milliseconds. **The load-bearing premise, stated explicitly because it is what makes a timeout
reportable HERE and not in the panic-fuzz oracle:** `generate.rs` bounds every loop by
construction — `LOOP_CAP`, a bounded `for`, a `while` with a mandatory increment on a reserved
counter — so a *generated* program that does not terminate in the timeout is a Chezzi bug, by the
same argument `generate.rs`'s own header uses to justify treating a Chezzi fault as real. The
panic-fuzz oracle fuzzes raw token streams, not correct-by-construction programs, so a hang there
can legitimately be a slow/malformed input rather than a bug — this verdict does not transfer. **If
`generate.rs` ever gains an unbounded loop, this verdict must be revisited.**

Fixed: on a chezzi timeout, `run_sources` now runs Python anyway. If Python exits **0**, before
reporting anything Chezzi is re-run **ONCE at 3x the configured timeout** — a false-positive guard,
required rather than optional: a single wall-clock timeout on a loaded machine is not proof of a
hang, this gate runs in CI beside a full `cargo test`, and the project has already had to rewrite a
wall-clock assertion for exactly this reason (commit `0fc437a2`). Only if the re-run also times out
is the outcome `Outcome::Divergence { kind: DivKind::ChezziHang, chz, py }`. If Python does anything
else (non-zero exit, itself times out, or the harness can't even run it) the outcome stays the
existing non-finding `Outcome::Timeout { which: "chezzi" }` (or routes to `HarnessError` if the
harness itself broke) — the program is then outside the shared subset, and a slow/hanging CPython
on our generated input is a harness/generator matter, not a Chezzi claim. `chz` in the reported
`Divergence` is a SYNTHESIZED `Capture` — `run_one` returns nothing on a timeout, so there is no
real capture to report — with empty stdout, a stderr note naming the timeout, and `code: None`.
That reuses the `code: None` bit pattern for a NEW meaning ("no capture, timed out"), which is safe
only because this `Capture` is built directly inside `run_sources` and is never passed through
`classify`: `classify`'s "a live `Capture` reaching me with `code: None` means a signal kill"
invariant (pinned by `W7-33`) is therefore untouched, not re-overloaded — spelled out in
`DivKind::ChezziHang`'s own doc comment so a future reader doesn't wire this capture into
`classify` and revive the exact hole `W7-33` closed.

**Review follow-up, same day: the retry itself had two more instances of this exact bug class.**
Adversarial review of this fix caught both — the retry's `match` was originally `Err(TimedOut) =>
Divergence, _ => Timeout`, and that wildcard silently absorbed two DIFFERENT outcomes into the
non-finding `Timeout`: (a) `Err(RunErr::CouldNotRun)` — the retry's OWN spawn failing — which
reproduces `W7-34`'s exact bug one call site over (a harness error must be fatal, not a scored
non-finding); (b) `Ok(c)` — the retry actually SUCCEEDING (a loaded-machine false alarm) — whose
resulting `Capture` was thrown away entirely instead of being compared against `py`, so a genuine
divergence that merely took 1-3x longer than the timeout went unreported. Fixed by extracting the
retry decision into `hang_retry_outcome` (a pure function taking `Result<Capture, RunErr>` +
`py` + `prog`, unit-testable without a subprocess race): `Err(TimedOut)` is the confirmed hang,
`Ok(c)` now routes through `classify(c, py, prog)`, and `Err(CouldNotRun)` routes to
`Outcome::HarnessError` like every other harness-broke arm in this file.

**F4 — `BothError` threw away stdout unexamined.** `classify`'s both-failed arm returned
`Outcome::BothError` (a non-finding) unconditionally, even when the two sides had printed
completely DIFFERENT stdout before failing (e.g. chezzi `b"1\n"` then a runtime fault, Python
`b"2\n"` then an unrelated exception) — ten lines of genuinely divergent output, discarded. Fixed:
the arm now compares the shared PREFIX of the two stdouts (`chz.stdout[..n] == py.stdout[..n]`, `n
= min(len, len)`) — a byte difference within the shared prefix is
`Outcome::Divergence { kind: DivKind::BothErrorStdout, .. }`; a length-only difference (one side
simply got further before its own unrelated fault) stays `BothError`. **Prefix-compatibility,
not equality, is the load-bearing choice**: CPython failing at PARSE time writes nothing to stdout
at all, while Chezzi routinely fails at RUNTIME after printing several lines first — under plain
`!=` that routine, harmless shape (`b"" != b"1\n"`) would false-positive on every such pair, which
is exactly the CPython-parse-error case this oracle sees constantly. Pinned by a test that asserts
the premise directly (`assert_ne!(chz.stdout, py.stdout)` before asserting the outcome still stays
`BothError`), so a regression to plain `!=` fails this test, not just silently ships.

Both new `DivKind`s render in `describe` (`src/difftest/mod.rs`). `ChezziHang` gets an explicit
line making clear chezzi produced no capture and Python exited 0 — without it, the empty
chezzi-stdout block sitting next to Python's non-empty one would read as an ordinary (and
misleading) stdout mismatch rather than a hang. `BothErrorStdout` was folded into the existing
`W7-30` byte-only-divergence hex fallback (previously keyed on `DivKind::Stdout` alone): two
byte-different stdouts that happen to DECODE alike can occur on this arm exactly as they can on the
exit-0 arm, and without the raw-hex line it would render as the same unreadable
verdict-contradicts-the-text-above defect `W7-30` fixed.

**Tests** (`src/difftest/run.rs`'s `#[cfg(test)] mod tests`): 3 for F4 —
`a_both_failed_run_with_divergent_stdout_is_a_finding` (`BothErrorStdout`, today `BothError`),
`a_both_failed_run_whose_stdout_is_a_prefix_stays_a_non_finding` (pins the naive-`!=`-would-flag-this
premise, then asserts the outcome stays `BothError`), `a_both_failed_run_with_empty_python_stdout_stays_a_non_finding`
(the CPython-parse-error shape). 2 for F3, using real subprocesses via `run_sources` — one is the
brief's own repro verbatim (`"while true:\n    x := 0\n"` / `"print(0)\n"`) under a 500ms `Config`
timeout: **measured wall time ≈2.02s** (≈4x the timeout, confirming the 3x re-run guard actually
ran before reporting); the guard test (both sides loop forever) stays a non-finding and finishes in
≈1s, without paying the 3x re-run since Python never survives to trigger it. A `#[cfg(test)]`-only
`locate_chezzi_for_test` finds the built `chezzi` binary via `current_exe()` rather than
`env!("CARGO_BIN_EXE_chezzi")`: this module is pulled by `#[path]` into TWO crates
(`tests/difftest.rs`, and `src/bin/difffuzz.rs` built as a test harness by a plain `cargo test`),
and the macro fails to COMPILE in the second one — confirmed empirically (`cargo test --bin
difffuzz` errors `environment variable "CARGO_BIN_EXE_chezzi" not defined at compile time`; Cargo
only defines that variable for integration-test compilation, not a bin built in test mode). Plus 2
more from the review follow-up, exercising `hang_retry_outcome` directly with synthetic captures
(no subprocess): `a_hang_retry_harness_error_is_not_silently_a_timeout` and
`a_hang_retry_that_succeeds_is_classified_not_discarded`.

All RED before the fix (assertion failures, not compile errors — the two new `DivKind` variants
were added first as inert data so the tests exercise real classification logic): GREEN after.
`cargo test --test difftest` — 46 passed, 1 ignored; the same tests also pass compiled into
`src/bin/difffuzz.rs`'s test harness confirming `locate_chezzi_for_test` works in both compilation
contexts. `cargo clippy --all-targets -- -D warnings` clean. Full `cargo test` (whole pre-commit
suite): 3878 lib + parity + conformance, difftest, difffuzz, panicfuzz, all green.

**The heavy sweep (seeds 0..3000, `cargo test --test difftest -- --ignored`) was run TWICE**, per
the false-positive-guard requirement — both runs **PASS**, **94.69s** and **94.50s**: no
`ChezziHang` or `BothErrorStdout` finding on either run, so the guard is not flaking under this
sweep. Reachability from today's generator is deliberately low (it avoids faults by construction,
so a Chezzi fault on a generated program usually implies Python succeeded cleanly rather than also
failing, and loops terminate quickly) — but `run_sources` is public and used by hand-written
probes too, and **Task 6 of this hardening series widens the generator to float arithmetic and
non-int calls**, which is expected to land in exactly these two arms.

### W7-37 — the differential generator emitted float arithmetic never, and called two thirds of the functions it generated never — **FIXED 2026-08-07**

**Found by:** an audit of what the CPython differential oracle *generates*. `W7-30`–`W7-36` audited
what it **reports** (a lossy decode, an allow-list downgrade, a signal kill, a spawn failure, a
hang, a both-failed stdout). This row is the other half: a verdict engine that is now correct on
every arm is still worth nothing over programs that cannot diverge by construction.

**The lesson, stated plainly: coverage that both engines agree on because neither executes it is
not coverage.** The proof is `W7-32`, landed four commits earlier in this same series. That was a
**real** Chezzi↔CPython divergence — a float's shortest `repr` broke an exact tie away from zero
where CPython breaks it to even, hitting ~0.5% of doubles — squarely inside the output-formatting
seam this oracle exists to own. It was found by a hand-written differential. This oracle could not
have found it, and `difffuzz --floats --seeds 0..1000000` would have reported a clean sweep, because
of the two gaps below.

**F6 — float arithmetic was never differentially tested.** `gen_float` took a `_depth` it ignored
and always returned a literal:

```rust
fn gen_float(&mut self, _depth: usize) -> Expr {
    // restricted to short exact-ish decimals (n/8) to dodge the formatting crossover
    let n = self.rng.range_i64(-80, 80);
    Expr::FloatLit(n as f64 / 8.0)
}
```

Four compounding consequences, each verified: `Expr::Bin { ty: Ty::Float, .. }` was never
constructed, so `emit_python.rs`'s float fall-through to native `/` was dead code and Chezzi float
`+ - * /`, float comparison and int↔float mixing were never exercised; `gen_assign` targeted only
`Ty::Int`, so a float was never mutated either; `float_lit` is **shared** by both emitters
(`emit_python.rs` imports it from `emit_chezzi`), so every float literal's text was byte-identical
*by construction* — the one thing a literal-only generator can emit is the one thing that cannot
differ; and `Features::full()` set `floats: false` anyway, so the CI gate never turned any of it on.

**F7 — most generated functions were emitted and never called.** `try_call` was only ever invoked
with `&Ty::Int` (from `gen_int`), while `gen_func` picked its return type from `rand_scalar_ty()`
(Int/Bool/Str, +Float) — so roughly **two thirds** of generated functions were emitted as dead
source. Any seed producing `fn f0(p0: str) -> str:` had no call site anywhere in the program. Same
for `try_index`: element reads on `List[str]`, `List[bool]`, `Map[_, str]` were never generated
because `gen_str`/`gen_bool` reached for `try_str_method`/`try_slice`/`try_membership` but never
for an index.

**Fix.** `src/difftest/generate.rs`:

- `gen_float` is recursive on `depth`, matching `gen_int`'s shape and guard discipline: leaves are
  float literals, in-scope float vars, `try_call(&Ty::Float)` and `try_index(&Ty::Float)`;
  composites are `Add`/`Sub`/`Mul` honouring `MAX_EXPR_DEPTH`.
- `gen_bool` and `gen_str` each gained a `try_call(want)` (gated on `feat.functions`, `p = 0.2`,
  matching `gen_int`'s call site) and a `try_index(want)` (gated on `feat.collections`, `p = 0.15`).
  `try_call`'s int-argument discipline is untouched — an int arg must stay a small literal because
  the callee's body and `ret_bound` were generated assuming `|int param| <= PARAM_BOUND`.
- `int_assign_targets` → `assign_targets`: floats are now mutable too. This needed **no** float
  bound system. The int path's `bound` exists to prove no i64 overflow; a float cannot overflow
  into a fault, and the one hazard it shares — an in-loop accumulator compounding geometrically —
  is closed the same way `gen_int` closes it: inside an in-loop `+=`/`-=` RHS, `gen_float` reads no
  float var at all (loop counters, the only loop-stable vars, are never floats). Its remaining
  leaves — literals, calls, index reads — are all loop-stable.
- `Features::full()` now has `floats: true`. It was flipped only after the sweep below came back
  green.

**Why turning float arithmetic on is safe, and where the brief's premise was wrong.** `n/8` leaves
are exact binary fractions, so `+ - *` over them are exactly representable — at `MAX_EXPR_DEPTH`
the worst case is 8 leaves, `80^8 / 8^8`, numerator under `2^53` — and both engines compute
IEEE-754 doubles, so the *values* cannot disagree. What can differ is **formatting**, which is the
point. But the brief's claim that "a mul chain drives magnitudes toward the scientific-notation
crossover" is **false as stated**: with `n/8` leaves a depth-3 product tops out near `1e8`, and
Python's crossover is `|x| >= 1e16` or `< 1e-4`. Shipping it that way would have been a second
round of coverage-that-reads-as-covered. So `float_leaf()` scales `n/8` by `2^e`, `e` drawn from
`[-70, -20] ∪ {0} ∪ [20, 60]`: a power of two moves only the exponent field, so it introduces **no
new rounding** and the exactness argument survives intact, while the range now straddles both
crossovers (`e` is bounded so an 8-leaf product cannot reach `inf` — worst case `~1e152` — or the
subnormal range). Verified directly: `print(0.125 * 8.470329472543003e-23)` and
`print(9.625 * 1152921504606846976.0)` are byte-identical between `chezzi run` and `python3`
(`1.0587911840678753e-23`, `1.1096869481840902e+19`).

**Deferred, in order.** (1) Float `Div`: a zero divisor diverges (CPython raises
`ZeroDivisionError`) and inexact quotients widen the formatting surface a lot at once — it wants
its own pass with a non-zero-divisor discipline like `gen_int`'s. (2) Int↔float mixed arithmetic
(`1 + 2.0`), which needs a coercion model in the IR the generator does not have today. Both are
commented at `gen_float`'s definition so the next reader finds them there, not only here.

**Tests.** `tests/difftest.rs` gained `fuzz_floats` (core features + `floats: true`, seeds 0..200,
kept as its own gate so a future `Features::full()` edit cannot silently take float coverage back
out) plus three structural probes in the shape of the existing `gen_emits_*` family —
`gen_emits_float_binop`, `gen_emits_non_int_call`, `gen_emits_non_int_index`. Those three are the
regression fence that matters: without them a refactor could revert to literal-only floats and
int-only calls and **every other test in the file would still pass**, which is exactly the failure
mode this row is about. All three are red against the pre-fix generator.
