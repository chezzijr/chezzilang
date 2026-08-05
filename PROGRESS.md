# Chezzi — Progress Tracker

Single source of truth for "what am I doing next." Update after every work session.

> **✅ W7-4a + W7-4b FIXED, W7-4d CLOSED as not-a-bug, W7-4c RE-SCOPED (2026-08-05) — the module
> SNAPSHOT path now keeps one cell per binding.** Two of W7-4's four shipped residuals were real
> wrong-answer bugs, measured against paired reference programs:
>
> | repro | before | after | CPython | Go |
> |---|---|---|---|---|
> | **a** — `l.GI := k.C.inc` and `main.GG := k.C.get`, two globals in DIFFERENT modules over one cell | `0` | **`2`** | `2` | `2` |
> | **b** — `p := [k]` (a captured local holding a module) reached by two sibling closures | `1` | **`3`** | `3` | — |
>
> Both are the documented sibling-sharing rule (`syntax.md` rule 2) failing *inside* one task — not F1
> isolation, which only says the PARENT must not see the write, and still holds.
> - **a** — `snapshot_modules` used one `WireMemo` **per module** and `fault_module` one rebuild map per
>   module, so a cell reached from two modules got two ids and rebuilt twice. Now one memo spans the
>   whole snapshot with only `emitted` cleared per module, which keeps each module **self-contained**
>   (it re-emits a shared cell's full definition under the SAME id; `from_wire_memo` dedupes first-wins)
>   — that is what makes LAZY fault order irrelevant and an untouched module free. The rebuild map moved
>   to `Vm::snapshot_rebuild`, swapped with the view, rooted in `collect`/`root_ctx`.
> - **b** — `SnapValue::Cell` carried no id. Now `Cell { id, inner }` + `Backref(id)`, minted from the
>   same memo as the wire arms, so a binding reached down BOTH the fast and slow paths keeps one
>   identity. A dangling `Backref` degrades to `nil` + `wire_backref_missing` (W7-11), never `.expect`.
> - **A second, unplanned fix fell out of b**, verified against the pre-fix binary: a recursive local
>   `fn` whose captures embed a module (`m := k` + `fn down(n): … return down(n-1)`) used to abort the
>   spawn with `maximum structural depth (10000) exceeded (cyclic data structure?)` (rc=1) and now
>   prints `41`, matching CPython. The wire path had round-tripped that cycle via `Backref` all along;
>   only the snapshot path faulted, behind a comment claiming the depth-cap walk "rejects cleanly".
>   **A tidy error message is not evidence the reject is right.** Fenced by
>   `airlock_handle_bearing_recursive_local_fn_round_trips`.
> - **d** — CLOSED as resolved-by-design. An `RwShared` copy-out view is per-piece: two `at()` calls ARE
>   two crossings. `get()`/`read()`/`slice` are one crossing and already share.
> - **c** — STILL OPEN, and **not** the same family as a. Attempted and stopped at a pre-declared stop
>   condition: `deep_clone_all` runs at `sched.rs:111`/`:192`, *before* `register_task`'s
>   `ensure_snapshot` at `:228`, so the first spawn of a view clones its cells before any snapshot id
>   exists — the side table the a-fix suggests is empty exactly when needed. Closing it needs the W6-2
>   pin instant hoisted ahead of the clone (an observable fault-ordering change), a monotonic id counter
>   so a mid-nursery re-snapshot degrades instead of colliding, and a per-task clone-id carry through
>   `QueuedTask`/`prepare_worker`/`prepare_serial_child`/`lower_task`.
>
> **Two lessons, both about PRICING a filed residual.** (1) W7-4a's ceiling comment predicted delicate
> rooting "across GC-visible points"; the `Vm`-lived map was needed, but the rooting is
> **belt-and-braces** — the new test still passes with the `collect` root line deleted (measured),
> because every entry is also reachable from the global it was `module_define`d into. (2) W7-4b's filed
> premise had gone **stale**: it named `Module`/`Native`/`Cffi`, but `Native`/`Cffi` cross by value now,
> so only `Obj::Module` forces that arm — and the code calls that "source-unreachable, defensive only",
> which reads as unpriceable. It is reachable (`m := k` binds a module to a local). **Re-derive a
> residual's premise against today's code before trusting its price tag.**
>
> **Verified.** `airlock_cross_module_shared_binding_is_one_cell` + `airlock_handle_bearing_cell_keeps_one_binding`,
> each on serial, M:N, and a MULTI-FILE gc-stress run (new `run_file_stress` — `run_capture_stress` is
> single-source and cannot reach the lazy per-module fault path). `cargo test --lib` 3831 green; `chezzi
> test tests/chz/` 297/297 on both engines; 29-test `airlock_` panel green; thread sweep `1/2/4/8` × 25
> runs, 0 wrong. **Perf**: the W7-4 snapshot stress (400 module-global closures × 1000 nurseries) main
> 2.585 s → 2.573 s, flat. Full write-up: `docs/gaps.md` **§W7-4a/b** and **§W7-4c**.

> **✅ W7-21 FIXED 2026-08-05 — a module global that HOLDS a function is now callable through the
> module.** `BARE := k.one` in `l.chz`, then `l.BARE()` in an importer: `type error (line 3, col 6):
> module 'l' has no member 'BARE'` (rc=1) → **`ok`, prints `1`** on M:N, `--threads=1` and `--serial`.
> Both owning ancestors accept the direct call and were re-run: CPython `pk.G()` where `G = _one` → `1`,
> Go `pkg.G()` where `var G = one` → `1`. Cause: `ModuleSig` splits its member surface into `functions`
> (declared `fn`s) and `values` (a top-level `let`/`:=`, whatever the type), and the CALL arm read only
> `functions` — so a `Ty::Func` sitting in `values` resolved as a value (`l.BARE`) but not as a call,
> under a diagnostic that denied the member existed at all. **Checker-only** (`src/checker/expr.rs`): a
> `values` fallback on the `fsig == None` path, calling a `Func`/`BuiltinFn` with STRICT `check_args`
> (no int→float widening through a function value — the same rule the fn-value and fn-field paths
> carry). The compiler and VM were already correct and are untouched. The lying diagnostic is fixed
> independently: an existing-but-uncallable member now says `module 'l' member 'N' is not callable (it
> has type int)`; a genuinely absent one keeps `has no member`.
>
> **Lesson: the obvious runtime test was GREEN BEFORE THE FIX.** This is `checker⊋compiler`'s sibling —
> a checker that rejects what the system executes — and the instinct for that family is "run it on both
> engines". But `run_file`/`run_file_parallel` bypass the checker, so the both-engine test passes
> pre-fix; it proves the *lowering* exists, never that the *rejection* is gone. Only a graph-level
> `check_graph` test is the fence (`checker::tests::module_global_of_fn_type_is_callable_qualified`);
> the VM test keeps its own job and its doc-comment now says which one it is.
>
> **Adversarial review added three things.** (a) A member whose own initializer errored (`X := k.nope`)
> is `Unknown`-typed and the first cut reported *"not callable (it has type ?)"* — a cascade asserting
> a type nobody knows; it stays silent now (**2 errors → 1**, matching `f := l.X; f()`). Not a
> regression — pre-fix emitted 2 as well — just no reason to keep it. (b) The arm records the editor
> HOVER, which the filing had written off because `record_method_hover` takes an `FnSig` a `values`
> member lacks — the member's own `Ty::Func` **is** what that helper builds, so it is one
> `hover_record_at` call (`editor::tests::hover_module_fn_value_member_call`, verified to fail with the
> line removed). "The helper doesn't fit" was a claim about the helper, not the feature. (c) The
> STRICT-vs-widening rule was **asserted by a comment and pinned by no test** — arity and `str`-vs-`int`
> fail under either helper. The deciding case is now asserted both ways: `l.FL(2)` (a fn VALUE) errors
> `expected float, found int`, `k.half(2)` (a DECLARED fn) widens. Full write-up: `docs/gaps.md`
> **W7-21**.

> **✅ W7-17 FIXED 2026-08-05 — `--timeout` now reaches a fiber PARKED on a timer.** A `timer(ms)` wait
> inside a `parallel:` nursery with no runnable sibling ran to its own deadline and then executed the
> statement after the wait — the exact fall-through a hard abort exists to prevent. `--timeout=300`:
>
> | shape (no runnable sibling) | before | after |
> |---|---|---|
> | nursery `timer(3000).recv()` | **3004 ms**, `FAIL … SWALLOWED` | **304 ms**, `TIMED-OUT t` |
> | nursery `wait:` with a `timer(3000)` arm | **3004 ms**, `FAIL … SWALLOWED` | **304 ms**, `TIMED-OUT t` |
> | serial cooperative `wait:` timer arm | **3004 ms**, `FAIL … SWALLOWED` | aborted |
> | Go `go test -timeout 300ms` + `<-time.After(3s)` | panics at 300 ms, `t.Fatal` never runs | — |
>
> Stable at `--threads=1/2/3/4/8`/default. Both park sites' timer jobs now fire at `min(their own
> deadline, the run deadline)` and deliver `true` only if their own deadline really passed; an early
> fire goes through `deadline_gap_wake`, and `chan_recv_step`/`op_wait_poll` gained a `--timeout`
> checkpoint (`Vm::deadline_halt`, split out of `block_halt_check`) so the re-check turns that wake
> into the abort. With the cap off it is byte-identical to the one-shot job it replaces: one wake,
> **no re-arming**, so W7-16's 200-re-arms/s cost is not paid here. A third site fell out of measuring
> the fix: the serial cooperative `wait:` timer arm was still a bare `thread::sleep` — the one
> inline-sleep W7-16 missed — now `block_until_deadline`; **N10 is unchanged** (it still takes the
> timer arm without yielding, it just observes the halts on the way).
>
> **Two things adversarial review changed, both invisible to a green suite.** (1) The early wake must
> leave STATE: `submit_at` runs *before* the fiber actually parks, so a bare wake landing in that
> window hits an empty bucket, is lost, and the fiber parks with its one job spent (`timer_armed`
> forbids a re-arm) — a hang past the deadline meant to prevent hangs. The park-gap re-check reads
> only {queued value, `closed`, `done_latch`, scope cancel}, and the first three would mean "the timer
> fired", so `deadline_gap_wake` trips the **cancel** and the deadline checkpoint is ordered above the
> cancel checkpoint to keep the verdict `timed_out`. (2) The first cut put that checkpoint at the top
> of both ops, ungated, which **silently truncated a `defer`'s cleanup `ch.recv()` on an already-queued
> value** — W7-16's own bug, re-introduced by its fix, passing every test including the new
> defers-still-run fence (whose `defer` only calls `print`). It is now suppressed by the same
> `deferring > 0` term `cancel_requested` uses, with the ungated check moved to the PARK.
>
> **The filed lesson was wrong, and that is the interesting part.** The row concluded this needed "a
> deadline-driven wake (a scheduler feature), not another checkpoint", because "chunk-re-arming only
> gets a wake, and the resumed fiber would re-park". Every clause true; the conclusion false — **a wake
> and a checkpoint are one fix**, and the predicted re-park is exactly what the missing checkpoint
> prevents. The scheduler-level alternative it pointed at is also *worse*: `flag_deadlock` drops parked
> fibers without `unwind_deferred`, re-introducing W7-16's skipped-`defer` bug, where wake-and-re-check
> faults from inside the VM and unwinds normally (fenced). And **a runnable sibling in every
> neighbouring fixture hid this for a whole milestone** — a spinner's own back-edge trips the deadline
> at 303 ms, indistinguishable in the report from the park being reached.
>
> Fences: `test_runner::timeout_aborts_a_sleeping_test_everywhere` (renamed from
> `..._on_every_block_in_place_path`, which was named that way *to* fence this by omission; 6 fixtures ×
> both engines, 2 red pre-fix), `a_timer_parked_task_aborted_by_the_deadline_still_runs_its_defers`,
> `the_deadline_does_not_truncate_a_defer_whose_recv_can_complete` (mutation-verified), and
> `a_live_timer_still_delivers_under_a_generous_timeout` for the other direction of the clamp.
> The one shape it left open — a fiber parked on the **netpoller** — is **W7-18, fixed the same day**
> (below). Full write-up: `docs/gaps.md` **W7-17**.

> **✅ W7-18 FIXED 2026-08-05 — `--timeout` now reaches a fiber parked on the NETPOLLER, the last shape
> that HUNG.** A nursery `spawn` on an untimed `l.accept()` nobody connects to produced no verdict and
> no output at all, killed by an external `timeout 10` at **10001 ms**. `--timeout=300`:
>
> | shape | before | after |
> |---|---|---|
> | nursery `l.accept()`, untimed | **10001 ms**, no verdict (external kill) | **304 ms**, `TIMED-OUT t` |
> | nursery `net.connect("192.0.2.1:9")` | same hang | **304 ms**, `TIMED-OUT t` |
> | the aborted task's `defer` doing `conn.write("bye")` | never reached | `TIMED-OUT t` + `DEFER-WROTE 3` |
> | **top-level** `net.connect` in the test body | 10 s spin, then `FAIL … SWALLOWED` | **304 ms**, `TIMED-OUT t` |
> | `accept(150)` under `--timeout=5000` | `Err("timeout")` at 154 ms | unchanged — still catchable |
>
> Stable 10/10 at `CHEZZI_THREADS=1/2/3/4/8`. Go agrees: `go test -timeout 300ms` against a goroutine on
> `net.Listener.Accept()` panics at 300 ms and never runs the following `t.Fatal`.
>
> **The filed premise was wrong, and that is the lesson** — a repeat of W7-17's lesson 1, one row later.
> The gap said this needed "a second marker distinct from `poll_timed_out`, threaded through the 5
> `PollPark` construction sites plus the re-inject". It needed none: `Vm::deadline` is already an
> absolute `Instant` on every worker and `Some` only under `--timeout`, so the resumed op just re-reads
> the clock. `PollPark`, `poller::register`, `next_timeout` and `fire_due_socket_timeouts` are untouched;
> the park registers for `min(op deadline, run deadline)` and `poll_timeout_check` decides which fired.
>
> **The obvious spelling of that idea re-introduced W7-16's skipped-`defer` bug on three paths**
> (halt-before-take, `?` past `demote_socket_exit`, clear-only connect resume), and adversarial review
> found a **fourth** the plan had mis-classified as a mere overshoot: a top-level `connect` handing the
> abort back as a *catchable* `Err`. All four shipped fully green. Fences:
> `test_runner::timeout_aborts_a_netpoller_parked_test`, `timeout_aborts_a_top_level_connect`,
> `a_netpoller_aborted_task_still_runs_its_defers`,
> `a_socket_timeout_is_still_catchable_under_a_generous_timeout` — all four behind
> `run_tests_timed_watchdog`, because a regression here hangs `cargo test` rather than failing it. See
> `docs/gaps.md` **W7-18**.

> **✅ W7-5d FIXED 2026-08-05 — a dead stdout no longer cancels sibling `Executor` jobs.** The bug was
> a **process-GLOBAL read inside a predicate that answers "is this ERROR a hard halt"**
> (`stream::out_dead_reason()` inside `Vm::executor_hard_halt`), so once stdout died every fault
> anywhere reclassified as a whole-queue kill switch. Repro: `Executor()` + a job printing until stdout
> dies + file-writing marker jobs, `| head -1`. Markers written, before → after:
>
> | engine | before | after |
> |---|---|---|
> | `--serial` | neither | both |
> | M:N `--threads=1` | neither | both |
> | M:N `--threads=2` | **either, run to run** | both |
> | M:N `--threads=3+`/default | both | both |
>
> **The same shape was in the tree twice, and fixing the first instance is what made the second
> visible.** `invoke_native`'s post-call `stream_halt` (`src/vm/call.rs`) reads the same global after
> EVERY native, so a sibling doing three `fs.atomic_write`s — never printing — faulted after the
> first, still varying by thread count and across runs. Its comment claimed "this only ever fires for
> the print natives"; nothing made that true. Now gated on a `Vm::stdout_writes` counter delta, i.e.
> "did THIS call emit to stdout" (which also stops a dead stdout faulting a FILE- or `stderr()`-backed
> `Writer`). Post-fix: **both markers 21/21 runs, all three writes 15/15**, every engine and thread
> count. `| head -1` contract intact (`rc=1`, `stdout closed (broken pipe)`); `--timeout`/`--max-heap`
> stay kill switches — a bound a sibling can outlive is not a bound.
>
> **Three lessons.** (1) Grep for the shape: a predicate over a value that also reads ambient state.
> (2) The ledger's proposed alternative — "an accepted-asymmetry test pinning what each engine does" —
> **was never available**; only measuring the thread-starved end showed the shape varied across runs
> at one thread count. An asymmetry you have not measured at both extremes may be a nondeterminism.
> (3) The first fence used markers making exactly ONE native call — the single shape where instance 2
> is invisible. When the contract is "the REST of the job runs", the fence needs a "rest".
>
> **Accepted cost, and note the primitive:** graceful `ex.shutdown()` only. A submitted job that never
> prints and never returns (`while true: j = j + 1`) now hangs `| head -1` where it exited in 4 ms —
> run-all keeping its promise, not a new uncancellable job class: **`shutdown_now()` still kills it in
> 54 ms** on every engine (a loop back-edge is a cancellation point). **CPython hangs
> identically** on `ThreadPoolExecutor` — the ancestor that owns `Executor` — so this follows it;
> **Go exits, via SIGPIPE on fd 1**, a signal policy Chezzi does not adopt (`stream_halt` records why:
> it would break `std.net`'s EPIPE contract). Nurseries are unaffected — `parallel:` aborts siblings
> on any fault by design, so the same program under `spawn` still terminates promptly on both engines
> (measured). Fenced by six tests in `tests/interactive.rs`
> (`dead_stdout_does_not_{cancel_sibling_executor_jobs,tear_a_multi_native_sibling}_*`), all asserting
> `rc != 0` + the pipe message so none can pass with `stream_halt` deleted. Full write-up:
> `docs/gaps.md` **W7-5d**.

> **✅ W7-5e FIXED 2026-08-05 — the gate W7-5d added can no longer be bypassed.** That gate asks "did
> THIS native emit to stdout" via a `Vm::stdout_writes` delta, which only `Vm::emit_out_bytes` bumped —
> so a new native reaching `stream::write_out` another way would emit bytes the halt cannot see, and
> `chezzi run x.chz | head -1` on a loop calling it would spin forever. `write_out` now takes the
> writing `&mut Vm` and bumps the counter itself: counting and emitting are one statement, and the
> bypass **does not compile** (`error[E0061]: argument #1 of type &mut vm::Vm is missing` — verified by
> writing it; it compiles pre-fix). Still per-`Vm`, so none of W7-5d's cross-job contamination returns.
> Zero behavior change: `| head -1` on a 100 000-line print loop exits at **4 ms, rc=1,
> `stdout closed (broken pipe)`** at default M:N and `--threads=1/2/4`; all 53 `tests/interactive.rs`
> fences green.
>
> **The lesson is in the filing, not the fix.** The row rejected the whole direction ("moving the
> counter into `stream::write_out` … would make it PROCESS-global") when only the `static`-beside-`OUT`
> *spelling* is global, and then ranked three fences that work around `write_out` instead — including
> one, "make it private to `exec.rs`", that **Rust cannot express at this layout** (no friend
> visibility, and `pub(in path)` names only an ANCESTOR module, never a sibling). When a filing rules
> out a direction, check it ruled out the direction and not one spelling of it: everything ranked
> below inherits the error.
>
> **Scope, from adversarial review:** this closes the VM's own sink, not fd 1. FFI reaching libc
> (`extern "libc.so.6": fn puts`) writes the descriptor directly, so `OUT_DEAD` never sets and that
> `| head -1` loop still spins — **6002 ms, no fault**, against 3 ms for the same loop using `print`.
> Pre-existing and untouched by this change; filed as **W7-20** and since **closed as not-a-bug**
> (below). Full write-up: `docs/gaps.md` **W7-5e**.

> **✅ W7-20 CLOSED 2026-08-05 — not a bug; FFI's stdout contract is now documented.** FFI writes the
> file descriptor itself, so the broken-pipe halt cannot see it and a `| head -1` loop of C writes
> spins. Running the owning ancestors settled it — **both do the identical thing**, on both
> observables:
>
> | loop under `\| head -1` | native print | the same loop through C |
> |---|---|---|
> | Chezzi | 4 ms, `stdout closed (broken pipe)` | **spins**, 6002 ms, killed, rc=0 |
> | CPython (`ctypes`) | 37 ms, `BrokenPipeError` | **spins**, 6001 ms, killed |
> | Go (cgo) | 2 ms, SIGPIPE | **spins**, 6001 ms, killed |
>
> Output ordering is byte-identical to CPython's too (`chezzi-1 chezzi-3 ffi-2 ffi-4` — the C library
> buffers its bytes until exit; `io.flush()` does not change it, 3/3 runs). So the fix is documentation:
> `docs/syntax.md` §12b gains the contract plus two runnable examples, with cross-references from
> `docs/stdlib.md`'s `std.ffi` unsafe-contract blockquote and its `print`/stdout guarantee list (which
> now ends by saying it covers the VM's own sink only). No code change, no test — there is no behaviour
> to regress and the only assertable shape is a hang.
>
> **The lesson is the filing's ranking, not the outcome.** It offered "flag the fd-1 writers at
> `extern`" against "leave it and document", calling the second *"cheaper and narrower"* — the budget
> option. Measuring inverted it: the flagger is not better-but-pricier, it is **wrong**, because it
> would drift Chezzi away from both ancestors on a surface where it already matches them exactly (and
> the symbol list is incomplete by construction — any C function can wrap `puts`). "Cheaper" was doing
> the arguing; nobody had run `ctypes`.
>
> **A second lesson, from adversarial review catching this entry's own first draft.** It claimed a C
> program cannot self-detect the dead pipe — *"`puts` + `fflush(NULL)` never reports it, glibc drops
> the per-stream error"*. **False.** Two independent prosecutors ran the shipped snippet and found the
> bug was in the extern declaration, not the C library:
>
> | `extern` declaration | `puts`'s value once the reader is gone | `if r < 0` |
> |---|---|---|
> | `fn puts(s: str) -> int` | `4294967295` | **never fires**, 200 000 iterations |
> | `fn puts(s: str) -> int32` | `-1` | fires at **i=1638**, 3/3 runs |
>
> Bare `int` marshals as C **`long`**; `puts` returns a C `int`, so the sign is lost and the guard dies
> silently — a trap `syntax.md` §12b already documents, walked straight into while writing that same
> file. The corrected number strengthens the ancestor match: CPython `ctypes` also detects at i=1638.
> Generalized: **"the library does not report X" is a claim about someone else's code made from one
> local observation** — the honest sentence is "my call did not observe X". Full write-up:
> `docs/gaps.md` **W7-20**.
>
> **✅ W7-14 FIXED 2026-08-04 — a `wait:` timer arm no longer swallows a sibling value that arrives
> first.** `WAIT-1`'s fix (`0b72ad60`) is gated on `self.mn.is_some()`, and a party that owns its OS
> thread — an eager `Executor` job, and the **top-level `main` thread** — has `mn == None`, so it fell
> into the cooperative inline-sleep: it slept to the timer deadline and took the timer without ever
> re-reading the siblings. The timeout arm beat the thing it is a timeout *for*, i.e. any `wait:` with
> a timer arm degenerated into a plain sleep. `timer(300)` beside a value at 50 ms: **`timer` @ 306 ms
> → `value 9` @ 56 ms**, matching the `parallel:` path (54 ms), Go's `select` and CPython. Not an
> eager-execution regression — pre-eager `main` (`b6cb9201`) measured the same 306 ms.
>
> **Fix (3 edits, `op_wait_poll` in `src/vm/netio.rs`, no new machinery):** add
> `timed_block = soonest.is_some() && owns_os_thread()` (the new `owns_os_thread()` is
> `is_counted_party()` minus its `native_reentry == 0` clause), gate the inline-sleep off for
> `can_block_in_place() || timed_block`, and clamp the block-in-place condvar wait to the soonest
> deadline (`DEMOTE_POLL_BACKOFF.min(deadline - now)`); the branch already re-polls, so the poll's own
> `now >= deadline` arm takes the timer, while before the deadline a sibling's value wins.
>
> **Adversarial review earned its keep twice here — read this before the next `wait:` change.** (a)
> The first cut gated on `can_block_in_place()` alone and shipped GREEN with a third path still
> broken: that predicate folds in `is_counted_party`, i.e. `native_reentry == 0`, so `main` inside a
> native callback (`[1].map(f)`, `Shared.update`, an FFI callback) still answered `timer` @ 308 ms.
> That clause is a rule about being JUDGED by the deadlock verdict, not about being able to block —
> hence the narrower `timed_block`, which admits an unjudgeable party only when a live timer arm makes
> the block provably finite (an untimed one still faults, else a hang replaces an honest verdict).
> (b) The first tests asserted `elapsed < 250 ms` against a 306 ms bug and FAILED inside a full
> concurrent `--lib` run (~2.2 s elapsed) while passing in isolation. They now use `timer(3000)` so
> the OUTPUT itself is the discriminator. **A timing bound whose fixed and broken values are within
> 6× of each other is a flake, not a fence.**
>
> **A second, unfiled bug fell out with it: a timer arm made an eager `wait:` UNCANCELLABLE**, and
> the job then ran the timer arm's body after the cancel. `thread::sleep` observes nothing and the
> cancellation checkpoint is at the top of the op, so `shutdown_now()` at 50 ms against a job waiting
> on `timer(3000)` printed `timer` and exited at **3007 ms**; it now prints nothing and exits at
> **57 ms**. The block-in-place path re-checks cancel / `--timeout` / the deadlock verdict once per
> tick. An inline sleep is a hole in every halt the loop it skips would have checked.
>
> Two lessons worth carrying. (1) The blocker on file — *"WAIT-1's recipe does not port: it submits the
> background deadline send into `self.mn`"* — solved the wrong problem: WAIT-1 injects a wake because a
> **parked fiber** has no thread; a block-in-place party *is* a thread and needs only a shorter timeout.
> (2) The clamp W7-13r(a) deleted as "dead code — `soonest` is provably `None` here" was **unreachable
> because of this bug**; the deletion documented the bug as an invariant. Ask *why* a branch can't fire
> before deleting it. Scope was widened past the filed row on purpose: `main` had the identical bug and
> no row of its own. `--serial`'s cooperative fiber keeps the inline-sleep (`gaps.md` N10, the frozen
> oracle — the one waiter that genuinely has no thread to clamp). Fences:
> `an_eager_wait_timer_arm_loses_to_a_sibling_value`, `a_top_level_wait_timer_arm_loses_to_an_eager_job`
> (both mutation-verified red without the gate), plus
> `a_timer_armed_eager_wait_is_cancellable_by_shutdown_now`.
>
> **✅ W7-16 FIXED 2026-08-05 — a wait whose DEADLINE WE OWN is a CONTINUOUS cancellation checkpoint.**
> `time.sleep_ms` and `timer(ms).recv()` now observe a cancel and the `--timeout` deadline for the whole
> duration of the wait, in a nursery, in an eager `Executor` job and on top-level `main`:
> `shutdown_now()` at 50 ms against `sleep_ms(3000)` went **3005 ms → 55 ms**, the post-sleep code no
> longer runs, and the cancelled task still unwinds through its `defer`s. A syscall-blocking native
> (`fs.*`/`request*`/`process*`/`io.*`) stays deliberately ENTRY-only — a `read(2)` already in the
> kernel is not ours to cut short. `--serial` has the same checkpoint but nothing to trip it mid-sleep
> (one thread), so it gains the `--timeout` half only. One shape was NOT reached and was filed as
> **W7-17** (a `timer(ms).recv()` parked in a nursery with no runnable sibling) — **fixed 2026-08-05**,
> see below.
>
> **Two filed premises were wrong, and measuring them is what found the real bug.** (1) "the same
> `sleep_ms` inside a nursery IS interrupted" — no: the parity fence passed only because its `boom()`
> faulted *before* `napper` entered the sleep. Delay the fault 50 ms and the nursery ran the full
> **3005 ms M:N / 3054 ms serial** and printed `napper woke`. There was no nursery-vs-executor split.
> (2) "`--timeout` cannot reach these jobs" was not executor-specific: `--timeout=200` against three 3 s
> sleeps (top-level, nursery, executor) reported **PASS** on all three — a documented hard-abort guard
> that silently never fired. And the contract resolved against CPython's `ThreadPoolExecutor` pairing:
> that is the *thread*-blocking sleep, while Chezzi's is a fiber wait whose ancestors (`asyncio.sleep`
> under a `TaskGroup`, Go's `select { <-time.After; <-ctx.Done() }`) both cancel — clinched by the fact
> that an eager job blocked on a plain `ch.recv()` *already* died at `shutdown_now()` in 56 ms.
> Fences: `tests/chz/stdlib/sleep_cancel_test.chz` (both engines, 2/3 red pre-fix),
> `a_sleeping_nursery_task_is_cancelled_mid_flight_by_a_sibling_fault`,
> `test_runner::timeout_aborts_a_sleeping_test_everywhere` (6 fixtures × both engines; renamed when W7-17 closed the park half), and the renamed
> + tightened `parity_blocking_native_is_an_entry_cancellation_checkpoint_on_both_engines`. See
> `docs/gaps.md` **W7-16**.

> **✅ W7-11 FIXED 2026-08-04 — the last ledger item that ABORTED THE HOST is gone.** An `RwShared`
> copy-out view (`at`/`slice`/`for_each`/`fold`/`get_key`/`has`/`for_each_entry`/`fold_entries`/
> `contains`) of an element whose cycle closes through the ROOT container killed the process on a
> legal, single-threaded, checker-clean program — `a.back = xs; RwShared(xs).at(0)` — while `get()` on
> the same box worked. The piece rebuild hit `from_wire_memo`'s `.expect`, because the id its
> `Backref` names is the container, which the view never copied. `elem_split` could not cover it: it
> re-emits CELL definitions per piece, and the missing node is a CONTAINER.
>
> **Fix:** `from_wire_memo` flags a dangling `Backref` instead of `.expect`ing, and the new
> `Vm::from_wire_piece` re-rebuilds the WHOLE container under **the caller's own read guard** and
> returns the piece by its wire id (`WireValue::node_id()`). The cycle survives, byte-identically to
> CPython (`copy.deepcopy(xs[0])` follows the cycle; `b.next[0] is b` → `True`; `pickle` agrees).
> Read `docs/gaps.md` **W7-11** before touching the airlock rebuild — especially the table showing why
> the W7-4 round-2 rejection of "rebuild the whole container" does **not** transfer (that one fired on
> every piece and re-read the box under a SECOND guard).
>
> Shipped alongside, requested in the same session: **`RwShared.at(i) -> Option[E]`** — it was the only
> `at` in the language that faulted, against `std.json.at` and its own `get_key`. `[]` stays the
> dangerous index; `at` is the safe one. Not the `min`/`max` → `Option` milestone.
>
> Method note worth keeping: the residual was documented in FOUR comments (including `WireMemo`'s own
> type doc, which named this exact shape) and fenced by nothing — no test ran a **cyclic** value
> through a **copy-out view**. A documented residual is not a fenced one.
>
> **▶ NEXT SESSION, START HERE (2026-08-04).** Branch **`eager-executor`** is **MERGED** into `main`
> (`5af067d9`, `--no-ff`): `217f9ffc` ships eager `Executor` execution (`docs/future.md` §2c),
> `5983af49` documents the follow-ups, and `0787e39d` closes **`W7-12`** — the one regression §2c
> introduced (a job blocked on a channel only its own joiner could fill hung on M:N). Its residuals
> (`W7-12r`) are since CLOSED by the process-wide quiescence detector — see item 5 below and
> `docs/gaps.md` section `W7-12r / W7-15`.
>
> 1. ✅ **Merged and re-verified on the merged-HEAD binary** (per `auto-task-review-unreliable`), not
>    just on the branch: full `cargo test` green (3794 lib + 121 across the other targets, 0 failed),
>    `cargo clippy -- -D warnings` clean, the W7-12 repro faults in 0s **byte-identically** on M:N and
>    `--serial`, and both false-alarm fences hold under repetition on the real binary — the bounded
>    cap-1 pipeline 30/30 and the live-sibling-producer shape 20/20, plus `--serial`. (Looping matters:
>    the predicate this replaced faulted only 2–7 of 30 runs.) `adversarial-review` had already run
>    three times over the branch, each round finding a real wrong-answer bug the green gate had no
>    opinion on; all fixed. The branch is deleted.
> 2. ✅ **`W7-13` FIXED 2026-08-04** — and its filed diagnosis was wrong, which is worth carrying
>    forward. It blamed a *missing* recv→sender wake and proposed a `wake_senders` on the eager pop
>    path; that wake already fires on all six pop paths, so the filed fix was a no-op. The real bug
>    was a LOST wakeup: `eager_wait_tick` handed a fresh `core.q` guard to `cv.wait_timeout` with no
>    predicate, so a `notify_all` arriving while the lock was free hit a condvar nobody was on yet.
>    `Condvar::wait_timeout_while` + the callers' own settle predicates closes it. Measured on the
>    release binary: the 50-handoff pipeline went from 7-of-15 runs paying an extra 5 ms quantum to
>    **all 15 at 3–4 ms**.
> 3. ✅ **`W7-13r(c)` FIXED 2026-08-04** — an eager job blocked on a full channel never observed a
>    `close()`. Pre-existing, not a W7-13 regression. It **HUNG** with no explicit `shutdown()`, and
>    with one it answered in 112 ms but blamed a *full* channel for a *closed* one. Now faults
>    `send on a closed channel` at **105 ms**; Go, compiled, panics at 104 ms on the same program.
>    Deliberate divergence recorded: `--serial` keeps `FULL_SEND_DEADLOCK`, because its drain runs
>    queued jobs one at a time and cannot interleave them. Precondition it does NOT fix: ≥2 pool
>    threads (`--threads=1` still hangs — `pool.rs`'s known fixed-size-pool hazard). Residuals (a) the
>    blind eager `wait:` poll and (b) `trip()`'s latch written outside `core.q` are ALSO fixed — see 4.
>    **`adversarial-review` caught a REGRESSION in the first draft of this fix**, and the green suite
>    did not: checking `closed` *before* the enqueue retry faulted the ordinary drain-then-close shape
>    (`a := ch.recv()` then `ch.close()`) that Go completes — the recv frees the slot for the blocked
>    sender, then the closer wins the race back to the lock. Retry first, check `closed` second; now
>    fenced both ways. Three of the measurements I first wrote into the docs were also wrong (a Go
>    `go run` cold-compile time quoted as runtime, a latency from the wrong program, and a false
>    account of what `--serial` does) — **re-run every number before it goes in a doc.**
>    **Two process lessons from this one, both from `adversarial-review`, neither caught by the gate:**
>    the first regression test used a process-global counter that neighbouring eager tests also moved —
>    it passed alone and on a lucky full suite, then failed at 24 beside its own neighbours, i.e. it
>    had already reported one false green (now an aggregate wall-clock bound, immune to that). And
>    three claims written into the new comments were false on inspection — always re-derive a
>    "this is already handled" claim from the code, not from the fix you just wrote.
> 4. ✅ **`W7-13r(a)` and `(b)` FIXED 2026-08-04 — the W7-13 family is now CLOSED.**
>    **(a)** the eager `wait:` block was a bare `thread::sleep`, paying a full 5 ms tick per wake-up.
>    Now waits on ARM 0's condvar with the tick as the timeout (clamped to the soonest timer
>    deadline). 300 blocking `wait:` wakeups, release binary: **1020/733/1102 ms → 5/5/5 ms**, same
>    answer. This is `demote_wait_block`'s existing four-line trick — the residual had been deferred as
>    "needs a shared multi-channel wait primitive, a design change of its own", which was simply false.
>    **(b)** `trip()` set `done_latch` outside `core.q`; now under it, matching `close()`'s discipline.
>    **Deliberately shipped WITHOUT a test**, measured 5–6 ms both ways: the window is the nanoseconds
>    between predicate-eval and condvar-enqueue, so a timing test would assert nothing and flake.
>    **A vacuous-test lesson worth keeping:** (a)'s first test let the producer race ahead, so every
>    `wait:` found its value already queued, the block branch was never reached, and it passed with the
>    blind sleep stubbed back in. Only mutation testing exposed it; a `gate` handshake forcing the
>    consumer to arrive first fixed it (0.01 s green vs 1.55 s red). **Mutation-verify every timing
>    test — "it passes" is not evidence it executes the code you think it does.**
>
> 5. ✅ **PROCESS-WIDE quiescence detector SHIPPED 2026-08-04 — `docs/future.md` §2d step 0, and it
>    closes `W7-12r` + a new `W7-15`.** Owner's call: *"we should not let it hang; what could be done
>    should be done."* `src/vm/quiesce.rs`. It did NOT lift `MnSched::is_deadlocked` (that stays
>    per-nursery, with every veto it earned intact) — it added a second, independent layer over the
>    parties that scheduler never accounted: `main` and each eager `Executor` job. `live = 1 +
>    Σ ExecutorCore::outstanding`; a party registers a `PartyWait` while blocked; deadlock ⇔ every
>    counted party is registered **and none of their waits is satisfiable**. Joiners are parties too
>    (`join_eager_jobs`), which is the node whose absence made `main`-in-`shutdown()` invisible.
>    It DELETED W7-12's predicate whole: `eager_join_deadlocked`, `join_has_no_live_siblings`,
>    `ExecutorCore::joining`/`blocked`, `JoinGuard`/`BlockGuard`, the `eager_block_suspect` debounce
>    and the registry sweep.
>
>    Measured (Go compiled + CPython, before writing code): (a) two blocked jobs in one executor,
>    (b) two executors deadlocking each other, (c) a blocked job with no `shutdown()` — all HUNG, all
>    now fault in <10 ms; Go reports (a) and (b), and for (c) neither ancestor faults (Go abandons the
>    goroutine, CPython hangs), so pairing Chezzi's CPython-style exit join with Go's verdict is
>    stricter than both, deliberately. **`W7-15`, new and previously unfiled**: `main` blocking on a
>    channel an eager job was about to fill used to FAULT where Go and CPython both print the value —
>    a wrong answer, not a hang, which by our own bar outranks (a)–(c) together.
>
>    **Five bugs found building it, none by reasoning** — three by the existing 300-handoff `wait:`
>    fence, and **two by `adversarial-review` on an already-green gate**. Carry these into §2d steps
>    1–4: (i) a party must not stay registered across its own retry (`pop()` and un-registering are not
>    atomic, so it reads as parked at the instant it made progress — faulted 6/10); (ii) **the verdict
>    must be ONE observation** — the first cut cloned the party list and released the lock before
>    reading channels, judging channel states against a party set that never existed at any single
>    instant; (iii) satisfiability ("is this wait already over?") replacing the debounce is a semantic
>    upgrade, not a tuning one; (iv) **`closed` means opposite things for a single `recv` (progress)
>    and a `wait:` recv arm (the poll SKIPS it)** — one variant for both was a HANG regression, rc 1 →
>    rc 124; (v) **a wait predicate that answers a CONSTANT is a bug waiting for a window** — `Join`
>    answering a flat "never satisfiable" faulted an already-drained `shutdown()` on a LIVE program,
>    2/20 runs. Both review findings now have mutation-verified fences. The generalisable rule from
>    (iv)+(v): a satisfiability arm must mirror what its own site SETTLES on, condition for condition.
>    Health fences all green,
>    cap-1 pipeline 0/40 false faults (the rejected progress counter was 6/40). Four new watchdogged
>    M:N tests, each mutation-verified. Residuals in `docs/gaps.md`: an all-joiner cycle, bounded-pool
>    starvation, partial deadlock (§2d step 3), scheduler parties (§2d step 2).
> 6. **Do NOT** apply `.superpowers/sdd/task-3-mn-half.patch` (wrong lifetime, superseded), and do NOT
>    re-grow a local per-executor predicate — step 5 replaced that wholesale.
>    And do NOT read W7-13's fix as licensing progress-rate reasoning in the detector: the `W7-13r`
>    `wait:` block still observes every non-arm-0 arm only once per tick, and `parked-is-not-stuck`
>    is a semantic objection, not a latency one.
>
> Still open and NOT part of this: `W7-5d` (hard halt mid-`shutdown()` engine asymmetry — note its M:N
> half is written against `run_workers_on_pool`, which eager execution DELETED, so re-derive it before
> closing it). The whole `W7-13` family is now closed. Two known limits it did NOT touch, both
> pre-existing and both recorded in `docs/gaps.md`: a blocked eager job holds its pool thread, so
> `--threads=1` still hangs two-job programs (`pool.rs`'s fixed-size-pool hazard), and nothing fences
> the no-hot-spin property of the eager send loop (`cargo test` measures verdicts, not CPU).

**Legend:** ⬜ not started · 🟦 in progress · ✅ done

> **✅ BUG-HUNT (2026-07-31, wave 7, gaps.md W7-8 — CLOSED) — a non-UTF-8 filename now round-trips:
> the `PathLike` protocol + the `path.Path` type.** `fs.list_dir`/`walk`/`glob`/`canonicalize` and
> `os.getcwd()` ran the OS bytes through `to_string_lossy`, so a name like `b"A\xffB.txt"` came back
> `U+FFFD`-substituted — a path that names **nothing**, with no diagnostic (`fs.exists` on it returned
> **false**). This was the LAST unswept member of the lossy-byte family (B1 / R1 / W6-4 / W6-9 / W6-14
> all previously fixed by giving the seam a `bytes` path); the family now has no unswept member.
> Design doc: `~/.claude/plans/2026-07-31-path-pathlike-design.md`.
> - **INPUT — `PathLike`**, a new RESERVED universe protocol (the 20th), sole method
>   `as_path(self) -> bytes`. `str`/`bytes`/`bytearray` satisfy it **intrinsically** (three rows in
>   `INTRINSIC_PROTO_METHODS` + a miss-only `("as_path", 0)` arm in `Vm::intrinsic_proto_method`, so the
>   W6-3 ratchet's generated probe passes on both engines); `path.Path` satisfies it structurally.
>   **NOT a breaking change:** `fs.exists("x")` / `io.open(f)` still compile with a bare `str` literal,
>   no annotation, no turbofish.
> - **OUTPUT — `path.Path`**, an ORDINARY Chezzi struct over `raw: bytes` (deliberately not a `native
>   struct`: that would have cost a `NativeRet::Struct` cross-module construction and a fourth
>   hand-maintained positional layout copy). DISPLAY and CONVERSION are two methods, as in Rust (whose
>   `Path` implements no `Display`): `p.str()` is LOSSY and never faults (`Stringable`), `p.decode()` is
>   EXACT with a recoverable fault, `p.bytes()` is raw. `os.getcwd() -> Result[path.Path]` — a CONCRETE
>   return type, which is what removes the erasure blocker that made `os.getcwd[bytes]()`
>   unimplementable (type args are erased before `Vm::call_native`).
> - **SEAM** — every path-taking native is now `_`-prefixed and typed `bytes` (`_exists`, `_list_dir`,
>   `_getcwd`, …, documented once in `docs/stdlib.md` as the internal byte seam); the public name is a
>   bodied pure-Chezzi wrapper doing `_native(p.as_path())` and re-wrapping into `path.Path`. All four
>   production decodes are byte-exact via `OsStrExt`, and `glob`'s matcher runs over `&[u8]` (so an
>   ASCII pattern matches a non-UTF-8 name; `?` still counts one Unicode SCALAR — see the panel
>   findings below). Lossy rendering survives ONLY in
>   human-facing error text (`Path::display()`) — the same semantics `p.str()` ratifies.
>   `is_blocking` strips a leading `_` so the D5 offload classification travelled with the rename.
> - **`std.path`** — all 10 lexical helpers moved `str -> str` ⇒ `PathLike -> Path` (option A), so a
>   non-UTF-8 name survives `basename`/`join`/`normalize` too. Ops CHAIN; convert once at the end.
>   New `bytes.decode_lossy()` (Python `errors="replace"`) backs `Path.str()` instead of hand-rolling a
>   UTF-8 substitution rule in Chezzi.
> - **THREE enabling front-end defects, all latent on main, all of the recorded
>   checker-superset-of-compiler class:** (1) `Compiler::collect_globals` reserved no slot for a
>   `native fn`, so a bodied fn in a native module could not call a native sibling — it PANICKED
>   `global '_exists' has no slot`; (2) the checker's native-module arm bound imports only inside its
>   `has_bodied` branch, i.e. AFTER `harvest_native_module` had resolved every signature, so a native
>   module's SIGNATURES could not name a type from a module it imports; (3) `Vm::do_method_call`'s
>   Module arm called the FRAME-FLATTENING `do_call` unconditionally — safe only while every module
>   member was a native, and `defer fs.remove_file(p)` (re-entrant, `NO_IC`, no running dispatch loop)
>   then ran off the end of the proto. It now takes the synchronous `invoke_value` path on `NO_IC`,
>   exactly like the struct/enum arms.
> - **Container invariance is UNCHANGED** and fenced: `List[int]→List[Any]`, `List[Sq]→List[Shape]`,
>   `Map[str,int]→Map[str,Any]`, `List[int]→Iterable[Any]` all still reject (the grant is a VALUE-level
>   early-out keyed on `Ty::Str|Bytes|ByteArray`, unreachable from any container element comparison).
> - **Two findings from the manual adversarial panel, both fixed in the same commit:** (a)
>   `os.temp_dir()` was STILL a lossily-decoded path (`$TMPDIR` is raw OS bytes, and
>   `.display().to_string()` threw them away) — a site W7-8's own report never named, through which a
>   `U+FFFD` path stayed constructible; it is `-> path.Path` now, so the "no unswept member" claim is
>   actually true. (b) porting `glob`'s matcher to bytes had silently made `?` count one BYTE instead of
>   one Unicode scalar, so `glob("a?c")` would have stopped matching `aéc` — a drift from Python
>   `fnmatch` / Go `filepath.Match`; `?` now consumes one full UTF-8 scalar wherever the name is valid
>   UTF-8 and degrades to one byte only where no valid sequence starts.
> - Tests: `tests/chz/stdlib/fs_bytes_roundtrip_test.chz` (the repro of record — 6 `test fn`s incl. the
>   spawn-airlock crossing), `tests/chz/stdlib/path_type_test.chz`, `tests/chz/spec/pathlike_test.chz`,
>   the migrated `tests/chz/suites/path_test.chz` (+ `t_byte_exact` / `t_pathlike_inputs_and_chaining`),
>   and Rust `ok()`/`rejects()` fences incl. the user-redeclaration diagnostic. Hand-verified on the
>   release binary on BOTH engines, byte-identical: `fs.exists` on the recovered name is **true** (it is
>   **false** on the pre-fix binary). No VM hot path touched, so no M19 bench moves.
> - **Three more findings from the second adversarial panel, fixed on the same branch:** (c)
>   `path.join` was `(parts: List[PathLike])` — and since Chezzi containers are INVARIANT (which this
>   change explicitly preserves), that signature was callable with a list LITERAL and **nothing else**:
>   no `List[str]` variable, no `path.join(s.split("/"))`, not even `fs.list_dir`'s own `List[Path]`.
>   A hard regression against main's `List[str]`, and the API did not compose with its own output. It
>   is **generic over the element type with a `PathLike` bound** now — `[T](parts: List[T]) -> Path
>   where T: PathLike` — which keeps every homogeneous list callable and touches invariance not at all
>   (fenced both ways in `pathlike_grant_does_not_widen_container_invariance` and by the Chezzi
>   `t_join_of_variables`). The literal-only test table that hid this grew variable-typed cases.
>   (d) Both doc sites for `glob` (`docs/stdlib.md`, `std/fs.chz`) still claimed `?` counts one BYTE —
>   the pre-panel behavior that finding (b) had already reversed; a user following them would write
>   `a??c` and match nothing. Corrected, and pinned end-to-end by a Chezzi test on a real `aéc.txt`.
>   (e) The byte-exact rewrite cost `std.path` **2.70×** against main's native-`str` module and shipped
>   with no `docs/benchmarks.md` entry. `bytearray.extend` (one native memcpy per piece) replaces the
>   per-byte `push` loops and one shared `_last_idx` backwards scan replaces three duplicate forward
>   scans plus a whole `_split` allocation in `basename` → **1.56× faster, 1.73× vs main**, measured
>   and recorded in `docs/benchmarks.md`. The residual is `_split`'s per-byte VM loop; `bytes` has no
>   native `split`, so a `bytes.split(sep)` (natural companion to the `ByteSeq` milestone) is the
>   named upgrade path.

> **✅ BUG-HUNT (2026-07-30, wave 7, gaps.md W7-9 + W7-10) — two stdlib paths that silently LOST bytes
> the program already had.** Both were "the data is gone and nothing says so"; both are now fenced by
> Chezzi-native tests (`tests/chz/stdlib/io_reader_carry_test.chz`, `csv_bare_quote_test.chz`, 10
> `test fn`s), identical on `run` and `run --serial`.
> - **W7-9 — `Reader.read_line`'s non-UTF-8 fault was DESTRUCTIVE.** It faulted recoverably pointing at
>   `read_bytes`, but the undecodable line was already consumed, so that very `read_bytes` returned the
>   *next* line. Root cause was the read shape, not a missing buffer: `BufRead::read_line(&mut String)`
>   takes the line off the `BufReader` and only *then* returns `InvalidData`. `read_line` now does
>   `read_until(b'\n')` + `String::from_utf8` and retains the raw refused line (terminator included) in
>   a new `ReaderCore::carry`, mirroring `SocketCore::carry` (`carry` OUTER, `inner` INNER, one
>   critical section). `read_bytes` drains the carry first without touching the fd (carry-only *short*
>   read); `close` discards it; every arm checks closed BEFORE serving it, so no leak past `close` and
>   no resurrection after EOF. All four read paths covered — the three native arms plus the pure-Chezzi
>   `lines()` generator, which inherits it for free. The fault is now **sticky** (a re-read re-faults
>   rather than skipping the bad line) and **self-healing** (a partial drain lets the remainder decode
>   as the next line) — the ratified `Socket.read` behaviour. `rest = Ok(b'A\xffB\n')`, was
>   `Ok(b'line3\n')`.
> - **W7-10 — `csv.parse` silently DELETED a bare `"` in an unquoted field** (`a,b"c` → `["a","bc"]`, a
>   third answer neither CPython nor Go gives). **Policy call: CPython** — keep it literally; Go's
>   `bare " in non-quoted-field` error was rejected because `parse -> List[List[str]]` has no error
>   channel. A per-field `field_start` flag in `std/csv.chz` gates the quote-opens-a-quoted-field
>   branch, so a quote elsewhere falls through to the ordinary-char arm: `a,b"c` → `["a","b\"c"]`,
>   `a,b""c` → `["a","b\"\"c"]` (two literal quotes — `""` collapses only *inside* a quoted field). The
>   quote-*starts*-the-field cases (`a,"b"c` → `["a","bc"]`, `"a"b,c` → `["ab","c"]`) already matched
>   CPython and are unchanged, now fenced. O(n) pre-collected-chars structure untouched.
> - **Also from the same P2 tier: W7-8** (`fs`/`os` lossily-decoded paths) — out of scope in THAT
>   session; it needed a `bytes`-carrying path seam, which landed **2026-07-31** as `PathLike` +
>   `path.Path` (top entry). W7-8 is CLOSED.

> **✅ BUG-HUNT (2026-07-29, wave 7, gaps.md W7-4) — two sibling closures over one captured local now
> keep ONE binding across the airlock; they used to silently split into two cells.** `Ctr(inc, get)`
> built over a factory-local `n`, sent through a `Channel` and driven on the far side, read `1` after
> two `inc()`s. **No concurrency needed to reproduce** (a plain `send`/`recv` round-trip inside `main`),
> and identical on `--serial`, on M:N, and at `--threads=1/2/4/8` — the parity oracle is *structurally*
> blind to it (both engines share one serializer). Every airlock arm reproduced: `Channel.send`,
> `Shared`, struct-field, `.iter()` cursor, `spawn f(g, h)` args, `spawn:` block capture, and the
> module-global snapshot.
> - **Root cause** (`src/vm/sched.rs`): `WireMemo` is deliberately **back-edge-only** — a node is popped
>   off the serialize DFS stack on exit — so an `Obj::Cell` revisited *off* the stack, which is exactly
>   what two sibling closures produce, was re-serialized as a fresh `WireValue::Cell` and `from_wire`
>   built two. Cycles round-tripped; shared bindings did not.
> - **Why it is a bug and not the DAG rule:** the off-path-alias-becomes-two-copies rule is documented
>   and deliberate **for DATA** (`docs/concurrency.md`; `pair := [xs, xs]` gives `2 1`, a knowing
>   divergence from CPython `deepcopy`). **A cell is not a data node — it is a BINDING's identity.**
>   `docs/syntax.md` already says a write through a capture is visible "across sibling closures", and
>   the crossing snapshot-copies a captured local into **one** per-task cell — one per *binding*, not
>   per reference. Go agrees (`f(); f(); g()` inside a goroutine prints `2`).
> - **Fix:** `Obj::Cell` alone moves to a **persistent** `WireMemo::cells` map (never popped); every
>   container and the closure VALUES keep the pop-on-DFS-exit `path` discipline, so the data-DAG
>   contract is byte-untouched. Plus one serialization per *logical* crossing wherever several roots
>   cross together: `do_spawn` (callee/receiver + args, via a new `deep_clone_all`), `do_spawn_block`
>   (all captures), `lower_task`↔`rebuild_ready` (captures **then** args — serialize order must equal
>   reconstruct order or a `Backref` hits `from_wire_memo`'s `.expect`), and `snapshot_modules`↔
>   `fault_module` (one memo + one rebuild map **per module**). `to_snap_depth`'s speculative fast path
>   ROLLS THE MEMO BACK when discarded — a discarded attempt must leave no cell id
>   (rebuild panic) and no `Backref` shortcut that could hide a handle from a later `has_handle`.
> - **Intended contract flip:** `airlock_aliased_closure_stays_independent` (`[bump, bump]`) →
>   `airlock_aliased_closure_shares_its_binding`, `1` → `2`. The closure *values* are still two
>   independent copies; the one *binding* they close over is now one cell.
> - **Stated ceilings** (`ponytail:` comments + `docs/gaps.md`), all the same shape — *two independent
>   serializations reaching one cell*: (a) **one task, two serializations** — a `spawn:` block's captures
>   and the module-global snapshot cross into the same task but are separate memos rebuilt at different
>   times (the snapshot faults in lazily), so a module global + a captured local over one factory-local
>   cell still split (fenced by `module_global_plus_local_capture_still_split`); (b) **cross-module** —
>   cell identity is per module in the snapshot; (c) **`RwShared` copy-out views** — `at`/`for_each`/
>   `fold`/`get_key`/`has`/`for_each_entry`/`fold_entries` rebuild one piece per step, so each piece is
>   an independent copy (a whole `get()`/`read()`, and `slice`, are one crossing and DO share); (d) a
>   cell whose inner value carries a residual module/native/FFI handle falls to `SnapValue::Cell`, which
>   has no `Backref` encoding.
> - **Review rounds 2–3 (2026-07-29) — four defects fixed on the branch.** (1) **CRITICAL, host PANIC +
>   silent wrong node:** a persistent cell memo makes a `Backref` legal BETWEEN SIBLING pieces of one
>   stored wire for the first time, and `RwShared`'s zero-copy read views DRAIN one stored wire through
>   many independent `from_wire`s — `RwShared([inc, get]).at(1)` aborted on `from_wire_memo`'s `.expect`
>   (no concurrency, both engines). Round 2 patched it by re-reading `core.v` to seed a whole-container
>   rebuild map, which was **worse**: the piece and the re-read came from TWO separate read guards, so a
>   concurrent `set` in the window (a write-preferring `RwLock` hands the lock straight to the queued
>   writer) resolved the piece against an unrelated serialization — reproduced as both the `.expect`
>   abort and a `CellLoad on a non-cell object` wrong-node abort, on M:N only, i.e. parity-blind. It was
>   also **O(n²)**: `for_each`/`fold`/… rebuilt the whole container ONCE PER ELEMENT (measured 3.7 s for
>   a 4000-element `for_each`, 34 s at 12000, versus 0.02 s on main). Round 3 deletes that whole path
>   (`from_wire_view`/`view_rebuild_map`/`WireValue::has_backref` are gone; `src/vm/netio.rs` is back to
>   main's `from_wire`) and fixes it at the SOURCE instead: `to_wire_crossable` — the single chokepoint
>   every cross-heap store routes through — serializes with `WireMemo::elem_split`, re-emitting a cell's
>   full definition once per **depth-1 subtree**, and `from_wire_memo` DEDUPES a repeated definition by
>   id. Every stored piece is then self-contained (no lock re-read, no whole rebuild), while a
>   whole-value rebuild (`Channel.recv`/`Shared.get`/`RwShared.get`/`slice`) still ties every reference
>   to ONE cell. Cost is a little wire size for a cell reached from 2+ depth-1 subtrees, nothing else.
>   (2) `do_spawn` now serializes **args before** the callee/receiver — the batch had flipped the
>   pre-refactor order, so a non-crossable callee pre-empted a non-crossable argument and the reported
>   fault message changed (`lower_task` already documents the same args-first rule). (3) `lower_task`'s
>   `wire_args` had been moved BELOW the callee classification purely for memo order, silently dropping
>   argument crossing-validation on the non-callable-callee path — moved back to the top (= main's
>   position); `rebuild_ready` reconstructs a `Closure`'s args before its captures to match. (4)
>   `to_snap_depth`'s speculative fast path cloned the whole `WireMemo` at EVERY node, making a module
>   with K cell-bearing globals O(M·K) — replaced by an exact rollback (restore `next_id`, drop ids
>   `>= next_id`, clear `path`/`gens_on_stack`), O(1) on the kept path. The two `WireValue::Backref`
>   docs that still asserted the pre-W7-4 invariant were corrected in the same pass.
> - **Perf (re-measured 2026-07-29, round 3).** `benches/run.chz` unchanged (no airlock on those
>   paths). 100k `Channel.send`/`recv` round-trips: main 127 ms → 124 ms. 20k-`spawn` storm: 221 ms →
>   217 ms. `RwShared.for_each` over 4000 sibling-binding closures: main 0.011 s → round-2 branch
>   **3.7 s** → round 3 **0.012 s** (the quadratic view rebuild is gone; fenced by
>   `rwshared_view_over_shared_bindings_is_not_quadratic`). Snapshot stress (400 module-global closures
>   over distinct cells × 1000 `parallel:` nurseries): main 1.084 s → memo-clone 1.243 s (**+15%**) →
>   after the rollback 1.110 s (**+2.4%** vs main).
> - **Where the rule stops** (checked, fenced, not a residual): identity holds within ONE crossing,
>   never BETWEEN crossings — two separate tasks over one local (two `spawn:` blocks, or two
>   `Executor.submit` calls) each still snapshot the binding independently, which is the documented F1
>   per-task isolation. A single `submit` whose one closure holds both sides WAS the bug and is fixed.
> - **Tests:** `tests/chz/spec/airlock_shared_binding_test.chz` — 7 arms + the `RwShared`-views
>   regression + a view run CONCURRENTLY with a writer (round 3; pre-fix it aborted the pool thread) +
>   the spawn args-before-callee fault-ordering pin + the discarded-snapshot-walk rollback fence +
>   **4 fences** (`[xs, xs]` through a `Channel` and across two `spawn` args, both must stay `2 1`; the
>   per-task-isolation boundary; the one-task-two-serializations ceiling), 15 tests under the
>   serial==M:N gate, also swept at `--threads=1/2/4/8`. Rust: the flipped fence, a new
>   `airlock_cross_arg_data_alias_stays_independent` (the seam the shared memo creates),
>   `airlock_module_global_shared_binding_survives_gc_stress` (the module-scoped rebuild map now lives
>   across `module_define`), and `rwshared_view_over_shared_bindings_is_not_quadratic` (the perf cliff).

> **✅ BUG-HUNT (2026-07-28, wave 7, gaps.md W7-2) — `Channel.close()` no longer loses the wakeup for a
> `wait:`-parked fiber, so a valid program stops faulting `deadlock:` on M:N.** A fiber parked in a
> multi-arm `wait:` whose channel was `close()`d concurrently was never woken (`--serial` 0/20;
> `--threads=8` 6/40, rising with parallelism); the detector then correctly reaped a genuinely
> unreachable fiber. **Root cause was NOT the reported one** — `close_wake` and `send_wake` share
> `wake_bucket` and its `Wait`-token CAS+sweep is fine. It was the gap re-check in `MnSched::park_wait`
> (`src/vm/mod.rs:2378`), whose recv predicate was `!g.is_empty()` and deliberately ignored `closed`
> (an in-code `parity-perf-0` note records an earlier `closed == ready` attempt reverted for
> live-locking). A `close()` landing between `op_wait_poll`'s empty poll and the park therefore woke an
> empty bucket. `send`/`recv`/`trip` all leave a signal the re-check DOES read, which is why only
> `close` reproduced. **Fix:** three-way arm accounting mirroring `op_wait_poll` — READY / DEAD
> (`closed && empty && non-timer` recv arm) / LIVE — requeue when any arm is ready OR every arm is
> dead. That requeue TERMINATES (the re-poll faults `wait: all channels closed`), so no spin; one dead
> arm among live ones still parks; the detector is untouched. Verified 0/60 at `--threads=8` (main
> 3/60), real deadlocks still reported.
>
> **✅ EAGER `Executor` — SHIPPED 2026-08-03 (`docs/future.md` §2c, decisions D1–D5).** `submit(f)` now
> **starts the job immediately** on the shared pool and `shutdown()` **waits** for it — the Python
> `ThreadPoolExecutor` / Java `ExecutorService` model, replacing the submit-only-enqueues drift that
> manufactured the never-run backlog, the reap-point question, the A2 auto-drain rescue and W7-5b.
> `--serial` deliberately keeps queue-at-`submit` (D3), so the engines differ only *between* `submit`
> and `shutdown()`; the usage/test rule is **read or assert after `shutdown()`, never between**.
> The W7-5 fault contract is inherited verbatim (D5): `shutdown()` hands submission-ordered outcome
> slots to the same `reduce_task_slots`, so lowest-index fault, hard-halt precedence and W7-5c's
> per-slot flush are unchanged and `executor_drain_test.chz` passes untouched.
> **W7-5b falls out FIXED**: the program-exit join now walks a heap-independent `ExecRegistry`
> (`Arc<Mutex<Vec<Arc<ExecutorCore>>>>`) that `spawn_worker` shares with every worker, so an executor
> created inside a task is reachable whichever heap made it — no change to `swap_ctx`'s `ctx.heap`-only
> gate, which is the STOP condition two previous attempts halted at. Verified against the pre-change
> binary: M:N lost both jobs, serial ran them. **A sibling bug fell out and is fixed too:** the exit reap
> iterated a snapshot of the executor list, so an `Executor` created by a job the reap was itself running
> was lost — silently on BOTH engines pre-change, and it would have become a live engine divergence if
> only the M:N side had been fixed. Both now re-scan until none is left un-shut.
> **The one real hazard was NOT the deadlock detector** (no predicate changed; `Executor` work stays
> outside it by decision D — and the rejected attempt's `exec_cores`/`exec_outstanding` post-mortem
> named identifiers that do not exist in this repo). It is that a job's blocking op falls to the
> "no scheduler" arms, whose `deadlock — no runnable task can send` verdict is true only while the
> submitter is stuck in the drain. Eager jobs now BLOCK there instead (`Vm::eager_block_recv`, a bounded
> poll on the channel condvar mirroring `demote_recv_block`'s settle order), which is what makes
> `submit(recv) … send … shutdown()` work rather than fault — the exact regression the rejected attempt
> shipped. Blocking `send`-on-full and `wait:` get the same treatment.
> **Regression this milestone introduced, and CLOSED in it — `W7-12`, see `docs/gaps.md`.** The
> blocking fix is right for a job waiting on a value `main` sends next line, and WRONG for a job waiting
> on a value only its own joiner could send (`ex.submit(fn(): ch.recv()); ex.shutdown(); ch.send(42)`):
> that faulted in 0s on both engines pre-eager and then HUNG on M:N while `--serial` still faulted. Now
> fixed by asking the one question the `netio.rs` arms could not: is this executor already being JOINED,
> with every job it still owes parked? (`ExecutorCore::joining`/`blocked` + `Vm::eager_join_deadlocked`,
> two consecutive observations required, existing fault text reused so M:N == `--serial` byte for byte.)
> Deliberately LOCAL and left narrow — four residuals in `docs/gaps.md` row `W7-12r`, chiefly that a
> program with no explicit `shutdown()` still hangs at the exit drain. The principled successor is a
> wait-for-graph (AND-OR knot) detector — designed in `docs/future.md` **§2d**, which also records why
> the "no scheduler ⇒ no sender" arms cannot decide this at all, and how Go/Python/Java/Rust/Erlang
> handle it.
> **Second hazard, found by self-review:** `submit` must not hold the executor's `core.inner` lock while
> a dispatched job runs — the GC's `Obj::Executor` mark arm takes the same non-reentrant lock, so a
> closure capturing its own executor deadlocks when the job's worker collects. Restructured to prepare
> the worker lock-free and hold the lock only for the allocation-free reserve; proved by restoring the
> bad order (`eager_executor_self_capturing_closure_survives_gc_stress_parallel` hangs).
> **Deleted with it:** `run_workers_on_pool` + `TaskSlots` + `DoneSignal` (the drain was their last
> caller). **Known limits, documented not fixed:** an executor job that calls `shutdown()` blocks a pool
> thread (self-join hangs, as in Python); and *main* is not an eager job, so a `recv` in main on a
> channel only a job will fill still faults instead of waiting. **W7-5d stays open.**
> Rewritten in the same commit: `examples/executor.chz` + `executor_autodrain.chz` (+ goldens),
> `module_global_freshness_test.chz`'s two drain-instant tests, A2/C5 prose, and the module-snapshot
> instant (crossing moves from drain time to **submit** time).

> **✅ W7-5 + W7-5c (the M:N `Executor` drain) — FIXED 2026-08-01: every queued job now runs, and
> `shutdown()` raises the lowest-submission-index fault.** The two earlier fix attempts logged here were
> superseded, not vindicated — see `docs/gaps.md`'s W7-5 session-log section for why both were rejected
> on a measurement (`os.exit` "0.006s → 18.9s") that turned out to reproduce identically on pre-fix
> `main` and is very likely a misattribution to an unrelated sleep-cancellability limit. The landed fix
> keeps run-all (an ordinary job fault no longer aborts its siblings — Python `ThreadPoolExecutor` / Java
> `ExecutorService` / Go `errgroup` all agree) and splits the drain's cancel flag in two: gone for an
> ordinary fault, kept for a HARD halt (`--max-heap`/`--timeout`) via a new
> `Vm::executor_hard_halt` predicate, so the resource-cap/`os.exit` kill switches survive
> un-swallowable. (That predicate ALSO carried a dead-stdout term until **W7-5d**, 2026-08-05, which
> removed it: a broken pipe is an ordinary per-job fault, not a whole-queue kill.)
> Early-stop is now opt-in in the caller via `std.cancel.Token` (`docs/concurrency.md` §6e/§8). Of the
> original rejection's four charges, three are answered above and by W7-5c below; the fourth — a
> faulting job leaves a non-terminating sibling unkillable — is **upheld and accepted by design**
> (run-all's whole point), not fixed; see `docs/gaps.md`'s W7-5 session log. **W7-5c**
> (a second faulting task's buffered output was silently dropped once two jobs could fault in one
> drain — latent under the old abort-on-first-fault semantics, live under run-all) is fixed alongside
> it: every faulting task's output now flushes at its task-order slot. Acceptance test:
> `tests/chz/stdlib/executor_drain_test.chz`, gated serial==M:N. Example:
> `examples/executor_results.chz`. Commits `0127cfd7`/`af3fb10b` (W7-5), `05204777`/`0611f8ae` (W7-5c).
> **W7-5b** (an `Executor` created inside an M:N task was silently discarded) is **FIXED 2026-08-03** by
> the eager-execution milestone below, not by the queueing model it was filed against. **W7-5d** (new, filed adversarially reviewing
> this doc pass): the run-all guarantee is for an ORDINARY fault only — a HARD halt mid-`shutdown()`
> stops `--serial`'s drain from popping the rest of the queue at all, while M:N has already dispatched
> every job before the halt can fire; the exact asymmetry is unverified under a thread-starved pool and
> has zero test coverage. `docs/gaps.md` OPEN ITEMS.

> **✅ BUG-HUNT (2026-07-28, wave 7 batch A, gaps.md W7-1/W7-6/W7-7) — three HOST-BOUNDARY fixes: the
> CLI no longer host-panics on hostile OS bytes, and `fs.copy` no longer eats a file.** All three live
> in the native/CLI seam where raw OS bytes become Chezzi values; none touch `src/vm/*` or
> `src/checker/*`, and none is a serial≠M:N divergence (the parity oracle was blind to all three).
> - **W7-1 (P0, DATA LOSS)** — `fs.copy(p, p)` returned `Ok(nil)` after truncating the file to 0 bytes:
>   `std::fs::copy` opens the destination `O_TRUNC`. It fired through a **symlink** too, so a
>   path-string compare is not a fix. `copy` (`src/native/fs.rs`) now tests **inode identity**
>   (`dev`+`ino`; `canonicalize` on non-unix) before the copy and returns a recoverable
>   `Err("… are the same file")` with the bytes untouched — Python `shutil.copyfile`'s `SameFileError` /
>   coreutils `cp a a`. A missing destination is never "the same file", so copy-to-a-new-path is
>   unchanged. Note it also refuses **hardlinked** distinct paths, which truncate identically.
> - **W7-6 / W7-7 (P1)** — `std::env::args()` and `std::env::vars()` PANIC on a non-UTF-8 item, so one
>   hostile argument, script path, or environment variable aborted `chezzi` with rc=101 at
>   `library/std/src/env.rs` **before the program started** — a host panic `recover:` cannot see, hitting
>   even a `print("hi")` program with no imports. Now `args_os()` (`src/main.rs`) / `vars_os()`
>   (`src/native/mod.rs`) with a **lossy** decode (invalid byte → `U+FFFD`; two raw env keys can collide,
>   last wins) — documented in `docs/stdlib.md §std.os`, not silent. `os.environ`'s sorted-by-key
>   lowering lives downstream in `src/vm/mod.rs`, was NOT touched, and its golden was re-run.
>   **A path is never taken from a lossy decode** (adversarial-review fix): `U+FFFD` substitution is not
>   injective, so a raw `sc\xffipt.chz` and a real `sc\u{FFFD}ipt.chz` decode alike — opening the alias
>   would silently run a DIFFERENT program with rc=0, strictly worse than the rc=101 it replaced. Any
>   path argument containing `U+FFFD` is refused (`reject_lossy_path`, `src/main.rs`) with rc=1, on
>   `run`/`check`/`ast`/`tokens`/`test`. **Stated v1 ceiling:** a script whose PATH is not valid UTF-8
>   cannot be run at all — threading a real `OsString` through would change `read_source`/`type_check`/
>   module-graph-root signatures (resolver + checker), its own milestone, out of scope here.
> - **Wave 6's meta-finding re-confirmed:** the panicking `std::env::args()` had **three** call sites,
>   not the one the report named — `src/bin/difffuzz.rs` and `src/bin/panicfuzz.rs` too. All swapped;
>   `grep -rn 'std::env::args()' src/` now returns zero live call sites.
> - Tests: `tests/chz/stdlib/fs_copy_test.chz` (4 `test fn`s — same-path, symlink, and two controls;
>   dual-engine-gated) + `tests/host_bytes_cli.rs` (3 spawned-process tests, `#![cfg(unix)]` — a host
>   panic is invisible to any in-VM assertion). **Deliberately NOT fixed here:** the separately-filed
>   lossy path DECODE (`fs.list_dir`/`walk`/`glob`/`canonicalize`, `os.getcwd`), which is uncoupled —
>   it landed later as W7-8 (2026-07-31, `PathLike` + `path.Path`; top entry).

> **✅ FIX (2026-07-28, gaps.md W7-3) — a `recover:` installed INSIDE a `defer` body now catches while
> the task is being torn down by a nursery cancel.** A cancelled task's `defer` that did
> `r := recover: panic(...)` lost the handler: the fault escaped and the rest of the cleanup was silently
> skipped (both engines, identical) — violating concurrency.md's "a `defer` is never itself cancelled"
> and Go's rule that a deferred function running during a panic completes and its own `recover()` works.
> Half-broken, which is why it survived: the same defer in an UNcancelled task worked, and a
> `?`-propagated `Err` in the cancelled task's defer was caught — only the fault/panic path broke. **Root
> cause** `src/vm/exec.rs:1189`, the post-step `Err` funnel:
> `if self.cancelled || rte.is_over_memory || rte.is_timed_out` bypasses the `recover:` handler stack,
> and `self.cancelled` is a task-wide LATCH still set while the cancelled task's defers run — the funnel
> was never gated on `self.deferring`, while the sibling predicate `cancel_suppressed()` (`exec.rs:1489`)
> already was. Wave 6's meta-finding shape again: **a fix applied to SOME arms of an N-way set**. **Fix:
> the (a) `self.cancelled` marker ONLY** — `cancel_bypass = self.cancelled && !(self.deferring > 0 &&
> caught_here)`, reusing the already-computed `handlers.last().frame_len > base_level` test (hoisted
> above the `if`). A defer body runs in its own nested `run_until`, so a handler installed INSIDE it owns
> the fault while one OUTSIDE sits at/below `base_level` and still cannot defeat the cancel; once the
> body finishes the pending cancel resumes travelling up (the task dies, the nursery still reports the
> original sibling fault, `rc` unchanged). **(b) `is_over_memory` / (c) `is_timed_out` deliberately keep
> bypassing UNCONDITIONALLY** — `chezzi test --max-heap` / `--timeout` aborts stay recover-proof inside a
> defer too, and neither ever sets `self.cancelled`, so the (a)-only gate cannot weaken them. Tests:
> `tests/chz/spec/cancel_defer_recover_test.chz` (RED-first driver + three fences —
> `recover_outside_defer_cannot_defeat_cancel`,
> `recover_outside_defer_cannot_catch_a_fault_raised_inside_it`, and
> `faulting_defer_does_not_swallow_lifo_next` (N6d) — serial==M:N gated) +
> `test_runner::recover_inside_defer_does_not_catch_timeout` pinning (b)/(c), whose load-bearing
> assertion is the ABSENT `SWALLOWED` marker, not the `TIMED-OUT` bucket (the outer abort re-stamps
> that bucket either way — adversarial-review fix). Also made `run_one_deferred` panic-safe: the
> `deferring` counter now restores on an unwind like `guarded`'s `native_reentry`, because since this
> change a leaked `deferring` would also permanently disable the cancel bypass. Honest limit: the
> `caught_here` conjunct is the conservative arm and no constructible program distinguishes it from
> `deferring == 0` — see `docs/gaps.md`. Docs:
> `docs/concurrency.md`, `docs/gaps.md` (new wave-7 session log; nothing added to OPEN ITEMS — ships fixed).
>
> **✅ RULING (2026-07-27, gaps.md W6-3d) — a NUMERIC newtype may no longer define an operator-named
> method; the declaration is a compile error.** `newtype Score = int:` with an `add`/`compare` method
> type-checked, then answered TWO different values for one protocol operation: `.add()` / a `[T: Add]`
> bound dispatched the user's method (the miss-only intrinsic never shadows one) while `+` auto-flowed
> to `int`'s native op — `99` vs `3`, silently, with `cmp(a,b) == 42` claiming `a > b` while `a < b` was
> `true`. Of three candidates, **(b)** (dispatch the method as the operator) had already been
> implemented and REJECTED — under a heterogeneous `List[Comparable]` a same-newtype pair takes the
> user's order while a cross-type pair cannot, so `<` becomes INTRANSITIVE with no fault — and **(c)**
> (drop the grant when such a method exists) leaves `+` diverging. **(a)** rejects the declaration,
> making the two-orders state unrepresentable, and matches Go (a defined type inherits its underlying's
> operators; Go has no operator overloading, so the conflict cannot arise there). Landed in
> `src/checker/setup.rs` beside the existing static-method reject, which defers for the same reason —
> the dispatch path does not exist. NARROW on purpose: ordinary methods are untouched, and non-numeric
> / generic newtypes are unaffected (`satisfies` already rejects the operator protocols for them).
> One-way ratchet, accepted: code deliberately calling `.add()` on a numeric newtype stops compiling.
> Tests: `checker::tests::numeric_newtype_operator_named_method_is_rejected` (all 7 names) +
> `…_ordinary_method_and_non_numeric_operator_name_still_ok` (the boundary); the old Chezzi pin that
> asserted the divergence was REWRITTEN (not deleted) as
> `numeric_newtype_operator_auto_flows_and_ordinary_methods_still_work`. Docs: `docs/syntax.md`,
> `docs/gaps.md` (W6-3d RESOLVED, index row removed — wave 6 carries no open DEFECTS; three disclosed
> residuals remain as their own index rows: `W6-9r`, `W6-10s`, `W6-10r`).
> **Corrected 2026-07-28 by adversarial review:** the first cut also rejected `neg`, which was WRONG
> and has been removed from the list. Unary `-` has no newtype path (`Neg` is never granted), so `-m`
> is already `cannot negate Meters` — there is no operator for a `neg` method to disagree with, and
> the rule was deleting working code under a false premise. All three prosecutors charged it
> independently; the defender confirmed by building both revisions. The reject now covers exactly
> `add`/`sub`/`mul`/`div`/`mod`/`compare` — the names a numeric newtype actually inherits an operator
> for — and an `ok()` boundary case pins `fn neg` as legal so the rule stays honest to its premise.
>
> **✅ FIX (2026-07-27, gaps.md W6-9) — `Writer.write_bytes` is byte-exact on `io.stdout()`/`io.stderr()`
> too; the VM's buffered output sink is now `Vec<u8>` end to end.** `io.stdout().write_bytes(b"\xff\xfe")`
> emitted `ef bf bd ef bf bd` (two U+FFFD) while the SAME method on a file writer emitted `ff fe` — Python's
> `sys.stdout.buffer.write` and Go's `os.Stdout.Write` both emit the raw bytes, so this was the last
> surviving member of the lossy-byte family (B1/R1/W6-4/W6-14, all previously fixed). **Root cause**
> `src/vm/fileio.rs` — the `Backing::Stdout`/`Stderr` arms of `write_to_core` called
> `String::from_utf8_lossy(data)`, because the sink was `&str`-typed. The `&str` signature was only the
> surface: the real constraint was **`Vm.out: String`**, the per-task buffer the whole serial-vs-M:N
> output-ordering seam is built on (`Vm`, `FiberCtx`, `WorkerResult`, all four `TaskOutcome` variants, the
> M:N join plumbing, and `reduce_task_slots`' task-order concatenation). **Fix: widen the sink to bytes end
> to end** — `Msg::Write(Vec<u8>)` + `stream::write_out`/`write_err(&[u8])` (`src/vm/stream.rs`); new
> `Vm::emit_out_bytes`/`emit_err_bytes` holding the logic with `emit_out`/`emit_err(&str)` kept as one-line
> wrappers, so every `print`/interpolation/native call site and the `Host` trait are untouched
> (`src/vm/exec.rs`); `out`/`stderr` retyped to `Vec<u8>` with `push_str` → `extend_from_slice` and the slot
> ORDER untouched (`src/vm/mod.rs`, `src/vm/sched.rs`); and the two `write_to_core` arms now pass `data`
> straight through. `Ok(data.len())` is unchanged and now truthful — no backing can short-write
> (`write_all` / in-memory / an unbounded queue) — which is also what Python returns. **serial == M:N holds
> by construction**: concatenating `Vec<u8>` per task slot in the same index order is byte-identical to
> concatenating `String`; nothing was sorted or normalised. The N1 dead-pipe contract is unchanged
> (`emit_*` stays a no-op that never touches `pending_exit`; `stream_halt` is still raised at the call
> sites) and W6-8's fd-2 poison-abort path is deliberately untouched. **The parity ORACLES compare RAW
> BYTES**, not the decoded capture: `from_utf8_lossy` is not injective (`ff` and `fe` both become one
> U+FFFD), so once a program can emit non-UTF-8 a decoded compare would report `parity OK` for a run
> whose engines put different bytes on fd 1 — the feature would have degraded its own detector. Both
> `chezzi run --check-parity` (`src/main.rs`, which also echoes the capture with `write_all`, so it
> reproduces the output of the command it checks) and the in-tree `assert_file_parity` now take
> `vm::run_file_bytes` → `vm::RunOutputRaw`. **Recorded residual:** the CAPTURE boundary (`Vm::take_out`,
> the `run_*` helpers, `RunOutput`) still decodes with `from_utf8_lossy` in one shared `captured()`
> helper, so `chezzi test` and lib embedders still show U+FFFD for a non-UTF-8 byte; that is a DISPLAY
> path, not a comparison one, and `chezzi run` (the only path a program's stdout reaches an fd) is
> byte-exact. Tests:
> `tests/interactive.rs::{stdout,stderr,buffered_stdout}_write_bytes_is_byte_exact_{mn,serial}` (real child
> processes — the in-VM runner captures as a `String`, so only a subprocess can witness fd 1/2) plus four
> in-language pins in `tests/chz/stdlib/io_writer_test.chz` (return count on stdout/stderr, the file arm's
> non-UTF-8 round-trip, a 200 KB write's full count), plus
> `tests/check_parity.rs::{check_parity_reports_a_byte_only_divergence,check_parity_echoes_the_captured_bytes_unchanged}`
> pinning the oracle itself (a channel-ordered program whose engines emit `ff`/`fe` in different order
> must still report DIVERGENCE and exit non-zero).
>
> **FOLLOW-UP — ✅ FIX (2026-07-28, gaps.md W6-9b): the oracle above was only HALF byte-exact.** Found by
> adversarial review of this branch. The two comparators W6-9 converted (`--check-parity`,
> `assert_file_parity`) are a MINORITY of the parity suite; three MORE cross-engine comparators kept
> diffing the lossy `captured()` decode — `assert_parity` (the ~82-site `run_capture` vs
> `run_capture_parallel` path), `assert_parity_file`/`parity_entry` (the multi-file + std-module oracle),
> and `parity_entry_cfg` (the `HostConfig` oracle). A detector gap, not a live divergence (no in-tree test
> emits non-UTF-8 through them), but `write_bytes` going byte-exact is what created the surface, so the
> blindness was invisible behind a FIXED claim. **Fix is strictly additive and helper-level — 0 call sites
> changed:** `src/vm/mod.rs` grew `run_program_bytes` / `run_capture_bytes` / `run_capture_parallel_bytes`
> holding the real bodies, with `run_program`/`run_capture`/`run_capture_parallel` demoted to one-line
> `captured()` wrappers (every public signature unchanged, so `src/vm/tests.rs`, `src/gc/tests.rs`,
> `src/checker/tests.rs` and `src/native/cffi.rs` are untouched); in `src/vm/parity_tests.rs` one shared
> `assert_stream_parity` does the TEXT compare first (readable failure) then the RAW BYTE compare ON TOP,
> `assert_file_parity` is deduped onto it verbatim, and the three blind comparators now route through it /
> `assert_outcome_parity` / `vm::run_file_bytes`. No existing assertion was removed, relaxed, sorted or
> made conditional. Failing-first proofs, both RED before the fix:
> `parity_tests::file_parity_catches_a_byte_only_divergence` (the real channel-ordered `ff`/`fe` program
> through `parity_entry` under `catch_unwind` — a CANARY on M:N slot ordering) and
> `parity_tests::outcome_parity_catches_a_byte_only_divergence` (direct on the helper; the capture path
> compiles standalone, so no real program can reach it with non-UTF-8). Disclosed residuals (`W6-9r`):
> ~31 hand-rolled `run_file_p` + `run_file` compares still diff decoded strings, `parity_entry_cfg_lines`
> keeps its by-design line-multiset stdout compare, and `vm_outcome`/`parallel_outcome` keep the `String`
> shape for the single-engine literal/`contains` sites.

> **✅ FIX (2026-07-27, gaps.md W6-8) — a STORED FFI callback aborts loudly instead of segfaulting.**
> **This was the last memory-unsafety in `docs/gaps.md`.** `signal(10, handler)`
> then `raise(10)` — checker-clean
> — used to give `rc=139` (SIGSEGV, core dumped, empty stderr) on both engines: `CallbackClosure::drop`
> `ffi_closure_free`d the libffi trampoline when the extern call returned, while C still held its code
> pointer, so the next invocation from C executed freed memory. Every C API that RETAINS a function
> pointer (`signal`, `atexit`, GLib/GTK, `pthread_cleanup_*`) was a guaranteed segfault, and no
> check-time reject is possible — the identical `fn(int) -> int` param is correct for `qsort`, which
> invokes the callback *during* the call. **Fix: leak the trampoline, POISON it.** `Drop` no longer
> frees; it clears an **`AtomicBool` armed flag** and leaks the `ffi_closure` + `Box<Cif>` +
> boxed `TrampolineCtx` (`ManuallyDrop<Box<…>>` fields). `callback_trampoline` now checks that flag
> **first** — ahead of the `params`/`ret` derefs and ahead of `catch_unwind` — and on a cleared flag writes
> `chezzi FFI: callback invoked after the extern call that received it returned; stored/cross-thread
> callbacks are not supported` to fd 2 and `abort()`s. Verified on the real release binary, both engines:
> `rc=134` (SIGABRT) with that message. **The cross-thread case is covered too:** the flag is atomic
> (`Release`/`Acquire`, so the trampoline's load never races the poison store — a plain `bool` there was
> a data race, and a foreign thread reading a stale `true` would have dereferenced a dead VM pointer),
> and the trampoline additionally compares `pthread_self()` against the `owner` recorded at ctx
> construction (write-once ⇒ race-free) and aborts on a mismatch with `…invoked from a thread other than
> the one that made the extern call…`. Every combination is defined: owner+during = live, owner+after =
> abort, any other thread = abort. The abort path calls **nothing but `write(2)` and `abort()`**, both
> async-signal-safe. An earlier cut drained `vm::flush_stream()` first so the program's queued stdout
> would survive; review rejected it and it was removed — that drain is an unbounded blocking rendezvous
> serviced only by the `src/vm/stream.rs` writer thread, so it HUNG (verified, both engines) when the
> poisoned callback fired on that writer thread, or when the writer was parked on a full unread pipe:
> no SIGABRT, no exit, no core — worse than the SIGSEGV it replaced. Buffered stdout is therefore
> discarded on this path, like on any crash. The message goes out through a short-count/`EINTR`/`EAGAIN`
> retry loop (one bare `write(2)` is dropped
> entirely on a non-blocking fd 2, leaving a bare SIGABRT with empty stderr). All three allocations must leak, not just the handle — libffi
> derefs the prepped `ffi_cif` and loads the userdata BEFORE our Rust fn runs, so freeing either would
> relocate the SIGSEGV into `classify_argument` (the `Box<Cif>` heap-pin bug again); `_cif` stays a `Box`
> under the `ManuallyDrop` and the compile-time guard asserts `&**c._cif`. `abort()` not a panic: the
> realistic site is a C signal handler, and unwinding into a C frame is itself UB. During-the-call
> callbacks are untouched — `examples/ffi_qsort.chz` is byte-identical to its golden on both engines and
> the whole `native::cffi` callback suite (fault re-raise, panic-caught, 2-/3-engine parity) is green.
> Only an **armed** trampoline leaks: `ctx.armed` is set as the last act before `ffi_call`, so a call that bailed
> during arg marshalling (interior-NUL `str`, return-only C type — all `recover:`-able) never handed C
> the code pointer and is still freed. **Accepted ceiling** (`ponytail:`-marked): one trampoline + CIF +
> ctx leaks per **callback-passing** extern call — ~400 B RSS, but as a W^X page PAIR out of libffi's
> exec pool, so it also eats `vm.max_map_count` (~1 VMA per ~130 calls); a `qsort` in a hot loop grows
> memory and mappings. That exhaustion is **defined**: the alloc goes through
> `libffi::raw::ffi_closure_alloc` with an explicit NULL check (`libffi::low::closure_alloc()`
> `assume_init()`s an uninit code pointer and feeds a NULL handle to `ffi_prep_closure_loc`), so a dry
> pool is the recoverable error `the FFI closure pool is exhausted`, never a crash. Upgrade path: one
> cached trampoline per (closure identity, signature). Callback-free extern calls never build a
> `CallbackClosure`, so no perf change there. Stored/cross-thread callbacks stay DEFERRED — the deferral
> is just loud now, on every thread. Tests: `tests/ffi_stored_callback.rs` — the repro, a cross-thread
> (`pthread_create`) callback, the aborts-without-hanging-on-a-full-unread-stdout-pipe check, the unarmed-free RSS-growth
> check, and a self-`RLIMIT_AS`-capped pool-exhaustion run — plus a `write_all_fd` unit test against a
> full non-blocking fd (subprocess tests: the repro dies on SIGABRT so it can never be a stdout golden,
> and FFI UB is layout-dependent; children run with `RLIMIT_CORE=1` so the deliberate abort leaves no
> core dumps). Docs: `docs/gaps.md` (W6-8 FIXED, open-items table row removed — no memory-unsafety left),
> `docs/syntax.md`, `docs/ffi-and-packaging.md §1b`.
> **✅ FIX (2026-07-27, gaps.md W6-7 + W6-10 — ONE root cause) — a wire core now caches its payload's
> GC summary, so the collector stops re-walking it and `--max-heap` finally sees it.** A value moved
> across the airlock into a `Channel`/`Shared`/`RwShared`/`Atomic`/`Executor` lives as a `WireValue`
> tree in an `Arc` **outside every `Heap`**, with no cached summary — so the GC treated it wrong in
> both directions. **W6-7 (perf):** `Heap::children` re-walked the ENTIRE tree on every GC pass, and
> because the threshold is object-COUNT based (`next_gc = 2*live`) while a big wire container is ONE
> heap slot, `live` stayed tiny → GC ran constantly → O(allocations × payload). **W6-10 (cap):**
> `live_bytes` counted only in-`Heap` slots, so a 195 MB channel backlog passed a 200 KB
> `--max-heap`. Both are now answered by ONE new walk, `vm::core::wire_summary` (beside
> `collect_core_gcrefs`, arm-for-arm), yielding `(approximate owned bytes, can-this-payload-root-a-heap-object)`:
> `children` skips a payload with no `Handle` and no nested core (O(payload) → **O(1)** per pass; a
> rooting payload is still walked in full, every pass, never memoized), and `live_bytes` adds the byte
> half. `wire_summary` is deliberately NOT `WireValue::has_handle()` — that answers the *airlock*
> question and calls the nested-core arms clean, which `collect_core_gcrefs` recurses into; caching
> its verdict would be a use-after-free. **The trap** (payloads are REPLACED by `set`/`update`/
> `write`/`store`/`exchange`/`cas`/`add`, so a stale CLEAN under-roots the GC) is closed three ways:
> `ChanState`/`ExecState`'s `queue` is now **private** so a missed push/pop site is a compile error;
> the single-value stores route through `SharedCore::store`/`RwSharedCore::store`/`AtomicCore::store`
> +`store_guarded`, refreshing the summary **under the same value lock** as the write; a
> `debug_assert` in `Heap::mark_core_payload` re-derives the verdict on every debug-build GC pass; and
> `vm::heap::replacing_store_refreshes_the_gc_summary` drives all four store methods on an
> already-memoized-CLEAN core with a handle payload, then mark-sweeps (mutation-verified RED when any
> `summary.set` is deleted — the Chezzi-level stress test CANNOT prove this, since `ensure_crossable`
> rejects handle-bearing values so no program can park a `Handle` in a core).
> `Default` is `WS_UNKNOWN` = walk-once-then-memoize, so a core built outside a store path (the
> `..Default::default()` constructors in `exec.rs`) degrades to the old behaviour, never under-roots.
> **Measured** (`--serial`, release, 200k-int container + n allocations): `RwShared` holder
> 0.447/1.946/7.916 s at n=100k/200k/400k (4.35× per 2× n — quadratic) → **0.069/0.203/1.101 s**, now
> tracking the plain-`List` control (0.061/0.196/1.203 s) at every n. Holder isolation at 200k:
> `RwShared` 1.766→**0.218 s**, `Shared` 2.051→**0.204 s**, `Channel.send` 2.050→**0.220 s** — the
> holder penalty is gone on the GC/read side. W6-7 itself needed no pacing change; no
> `benches/run.chz` movement (it uses no cores). **Round 2 fixed the fix's own
> two regressions** (adversarial review): `live_bytes` de-duped cores with a linear `Vec::contains` per
> core slot → O(D²) in DISTINCT live cores, on every `sweep()` (40 000 `Channel`s: 0.102→1.239 s) — now
> an `FxHashSet`, **0.109 s and flat in K**; and the `wire_summary` walk sat INSIDE the value lock for
> `Shared`/`RwShared`/`Atomic` (an exclusive-lock reader stall on the flagship read view) — the stores
> now summarise the caller-owned value **before** taking the lock, `store_guarded` takes the
> pre-computed summary. Store-side cost stays (one walk per store, +21% on 50 × `RwShared.set` of a
> 100k list) and is documented, not claimed away. The cost bought is **one `wire_summary`
> walk per channel `send`**, hoisted OFF the global `MnSched` lock (the `recv` side is free — each
> message's byte count rides in the queue next to it, so `pop` is O(1)): +7% on 2 000 round-trips of a
> 2 000-element list, +0.8% on 10× bigger messages, flat on a 4-producer M:N fan-out — measured, since
> `benches/run.chz` has no channel bench. `live_bytes` charges a core's bytes **once per core per
> heap** (`Arc` identity); per-*slot* charging multiplied a shared payload by the live-handle count and
> fired spurious OVER-MEMORY. One residual escape stays OPEN: a nested core with no surviving alias
> slot is counted nowhere (gaps.md `W6-10r`). **Round 3 fixed the half that was wrongly marked done:
> counting the bytes is useless if the cap is never SAMPLED.** `over_cap` is only assigned in
> `sweep()`, and `sweep()` only ran on a heap-OBJECT count — so a program pushing ~1 MB per `send`
> while allocating ~2 `Obj`s per iteration never swept and PASSED at 304 MB under an 8 MB cap (a
> 200k-int sibling: 3369 MB). `Heap::should_collect` is now byte-aware **when a cap is set**:
> `since_gc >= next_gc || (mem_cap != 0 && since_gc_wire_bytes >= (mem_cap/4).max(64*1024))`, charged
> at `Vm::to_wire_crossable` (the one helper every cross-heap store routes through) and reset in
> `sweep()` beside `since_gc`. With `mem_cap == 0` — every `chezzi run`, every bench, the whole
> parity gate — pacing is bit-for-bit unchanged and the charge is skipped entirely. Cost, measured: a
> capped store-heavy run pays a second `wire_summary` walk plus extra sweeps, **+11%** (1.649→1.828 s
> on 100 sends of a 200k-int list under a 4 GB cap); cap-off is noise (1.669→1.676 s). Residual
> SAMPLING escapes stay disclosed separately (gaps.md `W6-10s`): the documented inline-scalar loop,
> the by-hand airlock paths (spawn args, captures, `Executor.submit`), and a heap that only HOLDS a
> core. **Only observable change: `--max-heap`
> now trips where it previously passed** — which is the point of W6-10. Tests:
> `vm::core::wire_summary_bytes_and_dirtiness`/`wire_summary_state_transitions`,
> `vm::heap::core_payload_walk_is_memoized`/`dirty_core_payload_is_still_traced`/
> `live_bytes_counts_offheap_wire_payload`/`live_bytes_counts_a_shared_core_once_per_heap`/
> `live_bytes_sums_every_distinct_core`/
> `replacing_store_refreshes_the_gc_summary`, `vm::gc_tests::gc_stress_values_parked_in_cores`,
> `test_runner::over_memory_counts_offheap_wire_payload`/`under_cap_still_passes_with_many_handles_to_one_core`/`over_memory_trips_without_object_churn`,
> `vm::heap::wire_bytes_pace_a_sweep_only_under_a_cap`. Docs: `docs/gaps.md` (both retired FIXED;
> the sampling residuals kept in the open-items index as `W6-10s`, alongside `W6-10r`), `docs/benchmarks.md`, `docs/concurrency.md` (the read-view's
> O(1)-memory claim kept; the false *time* implication corrected), `docs/future.md §1b` (the cap now
> counts off-heap wire bytes — the inline-scalar escape in that same section is a DIFFERENT hole and
> remains OPEN).

> **❌ ATTEMPTED AND REJECTED (2026-07-26, gaps.md W6-3d) — candidate (b) for the numeric-newtype
> operator divergence makes `<` INTRANSITIVE. Branch discarded, `main` unchanged, the divergence
> stands.** (b) was "a numeric newtype's own `add`/`compare`/… dispatches as the operator too, so `+`
> and `.add()` agree". It does that — and breaks a deeper invariant. With a DESCENDING user `compare` on
> `newtype Ranked = int` and `xs: List[Comparable] = [Ranked(3), Ranked(1), 2]`, the branch answers
> `a<b`, `b<c` AND `c<a` all true — a strict cycle — where `main` gives a total order. Cause: a
> same-newtype pair takes the user's order, but a CROSS-type pair under the `Comparable` existential
> (`Ranked(1) < 2`) cannot — `compare(self, o: Ranked)` does not accept an `int` — so it falls back to
> the native ascending order. One list, two orders, no transitivity; `.min()`/`.max()` (which decide once
> per collection) then disagree with `<` (which decides per pair), silently and with no fault. **This is
> structural, not an implementation slip: (b) is incompatible with heterogeneous `List[Comparable]`
> unless such mixing is ALSO banned for a compare-defining type.** A second, ordinary regression showed
> the checker-side cost: requiring the bound protocol's `compare` second param to be literally `Self`
> after substitution broke a protocol whose `compare` takes the CONCRETE conformer type (compiles and
> prints `true` on `main`, rejected on the branch). Both re-verified by hand on the branch binary vs
> `main`, both engines. **Candidate (a) — reject the declaration — now leads**: it makes the two-orders
> situation unrepresentable instead of reconciling it after the fact. Needs a design ruling before any
> further attempt; do not re-run (b).

> **✅ FIX (2026-07-26, gaps.md W6-3c) — `Comparable.compare` is now TOTAL on a NaN operand, by the order
> `sort()` already uses.** The carve-out shipped a recoverable `cannot compare NaN (…)` fault; that string
> is gone. The `("compare", 1)` intrinsic arm's NaN branch (`src/vm/call.rs`) delegates to **`Vm::order_key`**
> — the single ordering site behind `sort()`/`sort_by_key`/`.min()`/`.max()` (`f64::total_cmp`, NaN
> deterministically at one end, numeric-`newtype` layers unwrapped, so `Meters(nan)` ≡ bare `float`). Net
> effect is the RULE COUNT: **one** total order shared by `compare`/`sort`/`min`/`max` and **one**
> documented divergence (total order for the method, IEEE for the operators) instead of two orderings plus
> a fault. `<`/`<=`/`>`/`>=` are untouched — `Vm::compare`/`ordered_bool` (`src/vm/arith.rs`) still answer
> `false` for every NaN comparison (IEEE-754/Python/Rust), and no checker change was needed
> (`(Comparable, float)` was already a paired intrinsic row). Two pinned corollaries: `x.compare(x)` is `0`
> for a NaN `x` although `x == x` is `false` (`total_cmp` on identical bits is Equal), and only the NaN
> branch routes to `order_key`, so a `±0.0` pair still compares Equal by the method even though `sort()`
> orders `-0.0 < +0.0`. Test: `compare_on_nan_uses_the_total_order` (rewritten from
> `compare_on_nan_faults_explicitly`, `tests/chz/spec/intrinsic_proto_methods_test.chz`) — it pins the NaN
> end RELATIVE to `sort()` plus antisymmetry rather than hardcoding `-1`, since the signbit of `0.0/0.0` is
> target-dependent (x86: NaN sorts FIRST). serial==M:N, verified on the release binary both engines.
> Docs: `docs/gaps.md` (W6-3c retired FIXED — 1 carve-out left, W6-3d), `docs/spec.md`, `docs/syntax.md`,
> `docs/stdlib.md` (incl. the honest note that `std.cmp`'s `min`/`max`/`clamp` are written with `<`, so
> they follow the operator rule, not the total order).

> **✅ FIX (2026-07-30, gaps.md W6-3e) — `Iterable[T]` works in TYPE position, not only as a bound.**
> `fn f(xs: Iterable[int])` type-checked, and `f([1, 2, 3])` conformed at the call site, but the body's
> `for v in xs` was rejected with `cannot iterate over Iterable[int]` — check-OK-then-broken, and
> backwards: the NARROWER `Iterator[T]` worked as a value type while the broader one did not. Root cause
> is a **representation asymmetry**, not a missing string: `resolve_type` intercepts the reserved name
> `Iterator[T]` into `Ty::Struct("Iterator", [T])`, while every other protocol name (`Iterable[T]`
> included) becomes `Ty::Protocol`; both iteration unions matched only the `Ty::Struct` spelling. Fix is
> **checker + one VM arm** (the compiler is untouched — the `for` lowering is type-erased and branches at
> RUNTIME on the heap `Obj` via `Op::IterableToCursor`). One
> `Ty::Protocol(n, args) if (n == "Iterable" || n == "Iterator") && args.len() == 1`
> arm in `iter_elem` (the arity guard keeps a BARE `Iterable` non-iterable), plus the two duplicated
> trailing `for`-binding arms collapsed into one that consults `iterable_elem` — so the iteration union is
> ONE predicate, closing the wave-6 "a fix applied to SOME arms of an N-way set" meta-finding rather than
> re-committing it. Every other consulter (the three comprehension arms, the `.iter()` fast path,
> `List()`/`Set()`/`Map()`, `satisfies(Iterable)`, `recover_iter_elems`) routes through those two helpers
> and inherited it, so an `Iterable[int]`-annotated param also forwards into `[S: Iterable[T], T]`.
> **The VM half — the same N-way set one rung down:** only the `for` lowering emits
> `Op::IterableToCursor`, so `List()`/`Set()`/`Map()`/`.iter()` inherited the static acceptance without
> the runtime conversion and faulted (`cannot iterate over struct (no `next` method)`) when the witness
> behind the annotation was an `iter`-only struct. That conversion is now the shared
> `Vm::iterable_to_cursor` (`src/vm/stmt.rs`), called by BOTH `Op::IterableToCursor` and
> `drain_iterable` — `iter_elem`'s declared runtime peer — so checker-accepts is a subset of
> runtime-can-lower again.
> `satisfies_args` grew ONE guard: a `Ty::Protocol` subject skips the intrinsic `Iterable` arm and is
> decided by the protocol-existential arm below it, exactly as `Ty::Param` already was — that arm is where
> the strict arg invariance lives. **Nothing widened**: `List[int]` → `Iterable[Any]`, `Iterable[int]` →
> `Iterable[Any]`, `List[int]` → `List[Any]`, `List[Sq]` → `List[Shape]`, `Map[str, int]` →
> `Map[str, Any]` all stay rejected (read-only covariance is deliberately out of the model), and
> `Iterable[T]` still cannot call `.next()` (W6-3b intact). Edge decided + fenced both ways: an
> `iter`-only struct passed to a param ANNOTATED `Iterable[int]` now WORKS (the annotation IS the element
> type), while the documented non-recovery under an `[S: Iterable[T], T]` BOUND is unchanged. 9 new
> checker tests (incl. 4 invariance fences that were verified GREEN pre-fix, so they pin behavior the fix
> must not move) + 5 `tests/chz/spec` `test fn`s covering list/set/map/str/cursor/generator/`next`-struct/
> `iter`-only-struct, a comprehension, the stateful-cursor drain, and the full cross product of
> `List()`/`Set()`/`Map()`/`.iter()` × the `iter`-only witness. serial==M:N, verified on
> the release binary both engines. Docs: `docs/syntax.md`, `docs/spec.md`, `docs/gaps.md` (W6-3e FIXED,
> the "Known limits" line scoped to BOUND position, plus a filed cosmetic diagnostic-wording drift).
> **Round 2** closed the protocol-SELECTION half of the same N-way set: the checker admitted a struct as
> `Iterable` by WELL-FORMEDNESS while the runtime picks by NAME PRESENCE, so a struct with a MALFORMED
> `next` plus a conforming `iter` was admitted via `iter` and then driven through the bad `next` —
> silently wrong elements (`[1, 2, 3]` instead of `[9, 9]`), or a nil bound into a declared-`int` param,
> identical on BOTH engines (parity-blind). `struct_iterable_elem` now refuses any struct that declares a
> `next` at all — `next` wins by NAME, exactly as `Vm::iterable_to_cursor` decides — so such a struct is
> non-iterable at CHECK time instead of check-OK-then-wrong. The collapsed `for`-binding arm's diagnostic
> was widened along with it too: a two-name `for k, v` over an `Iterable[E]` annotation (or an
> `[S: Iterable[T]]` bound) claimed "a struct iterator" with no struct in the program — it now names the
> type (`` `for k, v` requires a map, found Iterable[str] ``); a real struct keeps the struct wording.

> **✅ FIX (2026-07-26, gaps.md W6-3b) — `Iterator` now means CURSOR; a raw collection satisfies only
> `Iterable`.** `fn f[T: Iterator[int]](c: T)` accepted `f([1, 2, 3])` and then faulted at runtime with
> `type list has no method 'next'` — the checker keyed `Iterator` conformance on `iter_elem` ("can be
> iterated"), but `next` is STATEFUL and a raw collection holds no position (minting a fresh cursor per
> call would hand back element 0 forever, worse than the fault). Narrowed to the two forms that DO hold a
> position: an `Iterator[E]` cursor (`.iter()` / a generator result) or a struct with structural
> `next(self) -> Option[E]`; the new message points at `for`, `.iter()`, and the migration bound. The
> companion widening is what makes the migration possible: **element recovery now runs for `Iterable[T]`
> bounds too**, so `[S: Iterable[T], T]` accepts any iterable AND recovers `T` exactly like the old
> `Iterator` bound did — the accept set of the *iterating* form is unchanged. This is a **rename, not a
> retraction**: every shipped user of the bound (`examples/iterator_bound.chz`, `std.iter`'s
> `islice`/`imap`/`ifilter`) iterates with `for`, none called `.next()`, so all migrated to `Iterable`
> with **byte-identical** `.expected` output. Only `.next()` on a raw collection stops type-checking —
> and that was broken at runtime. Recovery is deliberately NOT total for `Iterable`: a struct with only
> `iter(self) -> Iterator[E]` still needs a concrete-arg bound (`[S: Iterable[int]]`). Retires the LAST
> `checker::proto::INTRINSIC_UNPAIRED` row — the const is now empty (both `vm::tests` ratchet loops stay,
> re-arming on the next carve-out). The 165-cell grant matrix
> (`vm::tests::intrinsic_grants_all_have_vm_arms`) went RED on the narrowing and green on the retirement,
> exactly as designed. 3 new checker tests (6 rejects + accept controls for cursor/generator/`next`-struct
> + `Iterable` recovery incl. a wrong-element boundary reject) + 1 `tests/chz/spec` `test fn` (both
> engines). Docs: `docs/syntax.md` (teaches `Iterable` vs `Iterator` = Rust `IntoIterator` vs `Iterator`,
> Go `range` vs an iterator value), `docs/spec.md` M13, `docs/stdlib.md § std.iter`, `docs/grammar.bnf`,
> `docs/gaps.md` (W6-3b retired).

> **✅ FIX (2026-07-26, gaps.md W6-12/13/14/15/17/18) — the wave-6 tail, six independent findings on
> disjoint seams, one batch.** All behavior-scoped (no new API surface); serial==M:N verified on the
> release binary for every repro.
> **W6-12** — `parse_iso8601` width-checks the YEAR like every other field (`"24-01-01"` parsed as year
> 24 while `"2024-1-1"` correctly `Err`'d). The bound is **4+ digits**, mirroring the emitter `pad_year`
> (>=4, more for an extended year) rather than Python's exact 4, which would reject this module's own
> output. **W6-13** — `days_in_month(y, m)` faults on a month outside `1..12` instead of returning a
> plausible-looking `31` that a `if d > days_in_month(y, m)` validator silently accepts. **W6-15** — a
> container now matches elements by **`identity or ==`** (CPython's rule) via one `Vm::elem_equal`, so
> one `nan` stored in a list is found by `==`/`in`/`index_of`/`unique`; the bare `==` operator is
> untouched (`nan == nan` stays false) and two separately-computed NaNs stay unequal. **W6-17** —
> the turbofish gate stopped over-rejecting the `RwShared` read-view's genuinely-generic
> `fold`/`fold_entries` (arm-only sigs, absent from the `structs` table the gate consults).
> **W6-18** — `io.open(<dir>)` `Err`s **at the call** with the OS's own wording, byte-identical to
> `io.read_file(dir)`, instead of handing back an `Ok(Reader)` whose every read fails. **W6-14** — a
> non-UTF-8 C string is a clean fault naming the bad offset and the `ffi.load_uint8_at` hatch, on
> `ffi.load_str`/`_at` and the extern `str`/`owned_str`/`str?` returns alike (was a silent U+FFFD
> mangle); `owned_str` still frees before the fault propagates. Chosen over doc-only to match the IO
> contract (`Socket.read` refuses a binary payload rather than decoding it lossily). Still OPEN and
> deliberately out of this batch: `List.min`/`max`/`min_by`/`max_by` faulting on empty while
> `first`/`last`/`pop` return `Option[T]` — a surface break, own milestone (gaps.md 2026-07-24 entry,
> re-confirmed 2026-07-26).

> **✅ FIX (2026-07-25, gaps.md W6-2 + W6-19, both P0) — a task now snapshots the module globals FRESH at
> its own `spawn`, at every depth; and a task's first-touch global WRITE no longer panics the M:N pool.**
> **W6-2** — `ensure_snapshot` memoized the `ModuleSnapshot` FOREVER (`snapshot_memo` was invalidated
> nowhere), so every later nursery/worker replayed the first nursery's frozen `Arc`: a global initialized
> *after* the first nursery replayed as `Value::nil()` inside later tasks (`print(n)` → `nil` at rc=0;
> `n + 1` → `cannot apply Add to nil and int`; `q.len()` → `type nil has no method 'len'`), and any
> between-nursery mutation was invisible. **The new rule: a task sees the module globals as of ITS OWN
> `spawn` — the Go rule — at every depth, including a NESTED `parallel:` inside a task, which sees the
> TASK's current view.** Per-task ISOLATION is unchanged (a task-side write never propagates out;
> `Shared`/`RwShared`/`Atomic`/`Channel` stay the only sharing), and an `Executor` job sees the globals as
> of the instant it STARTS — the `submit` on the default engine (eager), the drain on `--serial` (D3). `snapshot_memo` became a CACHE — dropped on any module-slot write (`set_global_slot` /
> `module_define`, the only two slot mutators) and dropped at every `Op::EnterNursery` when the cached view
> holds a mutable aggregate global (a conservative whitelist: in-place `q.push(1)` / `m[k]=v` / `p.x=1`
> writes no slot for a hook to see, so such a view re-snapshots per nursery while an all-immutable one
> keeps ONE snapshot for the run) — and it plus `module_snapshot` moved into `FiberCtx` so a shell draining
> several scopes faults each fiber from ITS OWN snapshot. A per-TASK PIN (`QueuedTask.snap`) keeps both
> engines snapshotting at the same program point — serial prepares a lazy nursery's tasks at its own join
> while M:N may early-enlist them at a nested join, or (an eager per-connection nursery) prepare at the
> spawn itself. It is resolved EAGERLY in `register_task`, from the cache, so the instant is the `spawn`
> statement on every path; a build failure is CARRIED on the task (`Result`) and raised where the task is
> PREPARED, which keeps a `break`-cancelled nursery faultless and the body's output ahead of the fault.
> Per-TASK, not per-nursery, because a bare `spawn` binds to the IMPLICIT nursery that spans a whole
> module/function body — any per-nursery pin freezes that body at its first bare `spawn` and re-creates
> W6-2's `nil` for every global declared later in it (the rejected first cut of this fix did exactly that).
> EAGER, because deferring the pin to "the next slot write, else the join" (the rejected second cut) made
> the M:N eager per-connection path pin at the spawn and the serial engine pin later — a byte divergence
> whose value also flipped with `--threads` — plus an O(pending-tasks)-per-write scan and an `Executor`
> isolation leak. Residual, documented: within ONE nursery consecutive spawns share one build, so an
> in-place aggregate mutation between two spawns with no assignment after it is not seen by the second task
> (each view is still ONE coherent instant, identical on both engines). Falls out: a shell needs no
> snapshot at all, so `spawn_shell` LOST its `snap` param, deleting 5 `ensure_snapshot` call sites and both
> `.expect("no fault possible")` teardown panic vectors. This **overrides decision G1** ("module globals are
> frozen under `--parallel`"), which was a memoization artifact, not design. **W6-19** (found while fixing
> W6-2, the wave's one serial≠M:N divergence) — `Op::GetGlobalSlot` faults the worker's module in, but the
> WRITE arms did not, so a task whose first global touch was `g = 99` indexed an empty `slots` vec:
> `thread 'chezzi-pool' panicked … index out of bounds: the len is 0` → `internal error: a parallel task
> panicked`, rc=1, while `--serial` printed the right answer. One `ensure_module_faulted` at the root of
> `set_global_slot`. **Tests:** new Chezzi suite `tests/chz/spec/module_global_freshness_test.chz` (20 tests
> — repro, between-nursery, aggregate in-place, the same-nursery pin instant (both flat and nested inside a
> task, i.e. the M:N eager path), bare `spawn`/implicit nursery, nested, `Executor` drain-instant + job
> isolation, the teardown/cancel matrix, and the isolation control; each test owns its own globals so no
> expectation depends on declaration order), 5 new parity tests (the per-spawn instant, bare `spawn` at
> module and function level), and 2 unit tests: the cache's BUILD COUNT per epoch (one build for N spawns
> in a nursery, one per nursery for an aggregate view, one per assignment) and the carried build error
> surfacing at task preparation; the two parity tests that PINNED the frozen rule were flipped to the fresh
> expectation with the reason recorded in place. **Docs:** `docs/concurrency.md`
> §2/§7 rewritten (the frozen-snapshot rule AND the long-retired G1 compile-error claim), `docs/spec.md`,
> `docs/concurrency-tier-d.md`, `docs/concurrency-b3.md` (annotated as history). **Perf:** the 9
> `benches/run.chz` benches moved only within noise (−4.5%…+2.6%, no nurseries in any of them); a
> 200k-nursery `--serial` loop is FLAT with scalar-only globals (0.598s → 0.594s: the cache short-circuits
> — one snapshot for the whole run, asserted by build count) and **+5.4%** with a `List[int]` global (one
> rebuild per nursery, the designed price of fresh-per-nursery). Spawn-storm shapes stay flat because the
> pin reads the cache: 3000 eager spawns with a 20000-element global is 0.014s → 0.018s (the rejected
> cut: 1.272s, 91×), and 40k spawns + 40k global writes 0.074s → 0.090s (rejected cut: 1.721s, 23×). One
> measurable regression, recorded: the same nested storm on `--serial` (3000 tasks × 20000-element copies,
> 10GB churn) is +10.6% from the changed allocation order — same build count (2), same peak RSS, and flat at
> realistic global sizes. Full table in `docs/benchmarks.md`; the precise per-mutation invalidation
> refinement (now that `src/vm/call.rs` is unfenced) is a recorded follow-up in `docs/gaps.md`.

> **✅ FIX (2026-07-25, gaps.md W6-1, P0) — `Writer.flush()`/`close()` on a `buffered` writer now
> actually persists.** `flush_core`'s `Backing::Buffered` arm returned `None` on an empty in-VM buffer,
> short-circuiting the recursion into the inner core — but a mid-write drain (a `write` larger than `cap`)
> had already `write_all`'d into the inner `BufWriter` *without* flushing it and emptied `buf`, so
> `flush()`/`close()` returned `Ok` and persisted NOTHING (the bytes only reached the fd at process-exit
> drop, so an in-process reader, a child process, or a SIGKILL saw an empty file). The arm now ALWAYS
> recurses (`src/vm/fileio.rs`); the WRITE, not the flush, is guarded by `!drained.is_empty()` so an empty
> `write_to_core` never hands `emit_out("")` to a `Stdout`/`Stderr` inner. Python
> `open(p,'wb',buffering=n)` / Go `bufio` semantics restored for a **file**-backed chain —
> observer-visible in-process, not just at exit (still not `fsync`, same caveat as `fs.atomic_write`; a
> `buffered(stdout())` flush only *queues*, on the never-awaited stdout writer thread — the docs now say
> so instead of promising universal visibility).
> Sibling arms enumerated, and **two of them were also broken**: (1) `WriterCore::Drop`
> (`src/vm/core.rs`) wrote its drained tail only when the inner was `Backing::File`, so a nested
> `buffered(buffered(create(p)))` chain lost its tail permanently — it now handles all four inner
> backings (`Buffered` → append to the inner's buf, which cascades on that core's own drop); (2) the new
> recursion made `WriteErr::Closed` reachable from a core BENEATH the receiver, which `writer_method`
> renders receiver-relatively ("flush on a closed writer") and `close()` MASKS — so both recursion sites
> now `map_err(from_inner)`, turning an inner `Closed` into `Io("the inner writer this buffer drains
> into is closed")`: the right handle is named and a flush that persisted nothing can no longer report
> `Ok`. The `Stdout`/`Stderr` lossy `write_bytes` was W6-9, a separate entry, NOT touched here — fixed
> 2026-07-27 (below).
> Tests: `tests/chz/stdlib/io_writer_test.chz` (8 `test fn`s — flush + close after a mid-write drain,
> never-filled control, exactly-at-cap, cap=1, a nested two-level chain, closed-inner `Err` on `flush`
> and on a draining `write`), serial==M:N, plus the Rust
> `vm::core::tests::drop_flushes_a_nested_buffered_chain_to_the_file` (drop timing isn't `assert`-able).
>
> **✅ FIX (2026-07-25, gaps.md W6-4, P0) — `std.process` gets its byte-exact hatch: `run_bytes` /
> `run_args_bytes`.** `std.process` was the arm R1's B1 sweep missed: `ProcResult`'s fields (and `cmd`'s
> return) are `str`, so child output went through `String::from_utf8_lossy` with no way to reach the real
> bytes (`A\xffB` → `b'A\xef\xbf\xbdB'`). Added `process.run_bytes(line) -> Result[bytes]` and
> `process.run_args_bytes(prog, args) -> Result[bytes]` — byte-exact stdout on success, and **`cmd`'s
> `Ok`/`Err` partition, not `run`'s**: `Result[bytes]` has no status channel, so ANY failed child is `Err`
> (stderr as the message, else `command exited with status N`, via a `failure_msg` helper now shared with
> `cmd`). That is the ratified R1 bytes-twin rule verbatim from `request.rs::lower_result_bytes` ("a
> non-2xx status here MUST become `Err` — otherwise a 404/500 HTML error page comes back as `Ok(bytes)`
> and a caller writes it to disk as if the download succeeded"): an `Ok(b"")` from a failed child is
> byte-indistinguishable from a command that printed nothing, so `run_bytes("gzip -dc missing.gz")` would
> write a 0-byte file as if it worked. Non-zero-with-meaningful-stdout (`grep`/`diff`) belongs on
> `run`/`run_args` (or `run_bytes("cmd; exit 0")`). Both registered `is_blocking` (D5 — else a subprocess
> wait pins a core M:N worker).
> **The text seam deliberately stays a documented lossy VIEW and does NOT Err.**
> The ratified B1/R1 contract is not "Err" but **non-destructive** (`src/vm/netio.rs` `decode_carry`: "a
> recoverable `Err` that silently drops already-received payload would just be a different flavour of the
> corruption B1 fixes") — `Socket.read` can only afford a strict `Err` because the bytes stay in
> `SocketCore::carry` for `read_bytes` to hand back. A finished child has NO carry: Err-ing `run` would
> destroy the captured stdout, stderr AND exit code (the bytes twins can afford `Err` precisely because
> they have no `code`/`stderr` to destroy), and the only "recovery" would be re-running an arbitrary,
> side-effecting command line. So `std.process` follows the in-tree precedent for a carry-less seam —
> `request.get`'s lossy `body: str` + byte-exact `request.get_bytes` (asserted on purpose by
> `request.rs`'s `into_string_corrupts_but_get_bytes_is_exact`). `run`/`run_args`/`cmd`'s Ok/Err contract
> is therefore UNCHANGED, so no caller is reclassified (e.g. `judge/run.chz`'s spawn-failure branch).
> **Residual:** the bytes path carries stdout only — a bytes-carrying structured result would need a new
> native struct through `seed_stdlib_structs` (checker-owned, out of scope). No `2>&1` workaround is
> advertised: it would splice stderr TEXT into the byte-exact stream, and `run_args_bytes` has no shell.
> Tests: `tests/chz/stdlib/process_test.chz` (6 `test fn`s; shell lines single-quote their temp paths so a
> `TMPDIR` with a space/glob can't word-split), serial==M:N.
>
> **✅ BUG-HUNT (2026-07-25, wave 6, `auto-task/extern-guards`) — the `extern "lib":` name-collision +
> struct-marshallability guards (gaps.md W6-5/W6-6/W6-11/W6-16, all FIXED).** Four filed defects, ONE
> checker seam (`src/checker/setup.rs`), two conditions changed — checker-only, reject-direction, no VM/
> compiler/native touch, so parity is structurally unaffected (nothing new runs). **W6-6 (P0)** — the
> extern/struct collision guard was DEAD CODE on the CLI path: the sweep looked up the bare `extern_names`
> spelling in `self.structs`, which the graph path keys MODULE-SCOPED (`main::S`), so `struct strlen` +
> `extern fn strlen` type-checked and silently called the **struct ctor** while `docs/syntax.md` promised a
> reject (the tracked `checker-test-helper-key-divergence` class — the bare-keyed `rejects()` helper passed).
> Fixed by keying the sweep off **`struct_names`**, the BARE-visible ctor set (the same predicate the
> nested-fn guard uses), which is bare in BOTH paths. Deliberate delta: an UN-imported stdlib layout name
> (`Match`/`Response`/`ProcResult`/`FileInfo`, parked bare + un-licensed in `self.structs` by
> `seed_stdlib_structs`) is no longer rejected — with no `import std.regex`, bare `Match(...)` is `unknown
> type 'Match'`, so nothing shadows the extern; `import std.regex` still licenses the bare ctor and still
> rejects. Both directions pinned. **W6-11** — `Ok`/`Err`/`Some`/`None` are now rejected too (builtin
> variant ctors, absent from `variant_owners`); `Result`/`Option` stay **accepted** — probe-verified
> reachable as extern names (a TYPE name is not callable), same rule as `extern_named_after_enum_type_ok`.
> **W6-5 (P0)** — a ZERO-field struct at an extern boundary passed `check` then PANICKED the VM
> unrecoverably (libffi `prep_cif: Typedef`, `recover:` can't catch a Rust panic); now one guard in the
> shared `struct_fields_marshallable`, so it covers the param AND return direction, rendering the BARE
> struct name. **W6-16** — the duplicate reserved-name diagnostic (`str`/`bytes`/`bytearray`/`Channel`/
> `List`/`Map`/`Set`, doubled LSP squiggles) FELL OUT of the W6-6 fix: the second report came from those
> prelude `native struct` layouts sitting bare-keyed in `self.structs`; every reserved name now reports
> exactly once (also under `--errors=json`). +5 checker tests (graph/entry path where the bare-keyed helper
> is blind, both decl orders, imported-vs-un-imported native struct, the four variants, `Result`/`Option`
> ok, once-only count, zero-field param+return+entry). Controls re-verified on `target/release/chezzi`,
> `--serial` == default M:N byte-identical: `examples/ffi{,_struct,_str,_int}.chz`, a plain `extern strlen`,
> and a `struct Cplx{re,im}` + `cabs` by-value-struct extern. Docs: `docs/syntax.md` extern section (the
> promise now true + names the variant ctors, the accepted TYPE-name cases, and the zero-field limit).
> **Fifth arm, folded in (`7abe925`) rather than deferred:** the first cut left `newtype N = int` +
> `extern fn N` OPEN as "its own follow-up" — but two of three adversarial reviewers charged the
> deferral, and rightly: a newtype registers a bare-visible one-arg ctor exactly like a struct, so
> `newtype abs = int` + `extern fn abs(x: int) -> int` checked OK and then called the CTOR, printing
> `abs(-7)` instead of `7` on both engines (verified on the real binary). Leaving a KNOWN instance of
> the class inside the very predicate being rewritten is not a follow-up — `newtype_names` joined the
> predicate, so it is now the whole bare-visible ctor set (`struct_names` + `newtype_names` +
> `variant_owners` + builtin variants). Test: `extern_named_after_newtype_rejected` (both decl orders,
> single-module + graph path, plus a non-colliding control).

> **✅ TEST RUNNER (2026-07-24) — `chezzi test` selection + output ergonomics batch (docs/future.md §3b
> #5/#6/#7).** Seven opt-in, low-risk CLI+formatting flags on the runner — NO VM/checker/compiler touch,
> pure `cmd_test` (flag parse) + `test_runner.rs` (formatting/filtering/timing). **Central refactor:** a
> `RunOpts { max_heap, timeout_ms, filter, fail_fast, show_output, json, verbosity, color }` struct + a
> `Verbosity { Normal, Quiet, Verbose }` enum; the new core is `run_tests_opts(root, parallel, opts)` and
> the three legacy positional fns (`run_tests`/`run_tests_capped`/`run_tests_timed`) are now thin shims
> over it (`RunOpts::default()` = every feature OFF). **The load-bearing invariant: `RunOpts::default()`
> reproduces the pre-batch output BYTE-FOR-BYTE** — every clause is gated on its field being non-default,
> so the no-flag render path is unchanged and the `chz_suite_passes_both_engines` byte-identity gate stays
> green. **Flags:** `-k`/`--filter <substr>` (run a subset by displayed name, filtered at the invoke site
> so they don't run; `(K filtered out)` summary clause; zero-match = `— no tests matched '<pat>'` failure);
> `--fail-fast` (stop at first non-pass, deterministic order: sorted files → free tests → suites, all
> declaration order); `--show-output` (surface a FAILING test's captured stdout indented under its line;
> default discards; pass never shown); `--errors=json` (machine output mirroring `check`/`run`'s flag —
> `{tests:[{name,file,line?,status,duration_ms}],totals:{…}}`, suppresses human lines); `-q`/`-v`
> verbosity (`-q` dots `.`/`F`/`E`/`M`/`T`, `-v` per-line + per-test `(Nms)` + total; mutually exclusive);
> `--color=auto|always|never` (auto = isatty on stdout, resolved in `cmd_test` so the runner never emits
> ANSI under the captured harness). **Timing is `-v`/json ONLY — never in default/quiet** (non-deterministic
> → would break the byte-identity gate). +9 runner unit tests (filter/zero-match/fail-fast/show-output/
> json/verbose-timing/quiet-dots/color); all 33 runner tests + the full suite green. Docs: `docs/future.md
> §3b #5/#6/#7` marked SHIPPED, `docs/gaps.md T4/T6` gaps struck, USAGE updated per flag.

> **✅ TEST RUNNER (2026-07-24) — `chezzi test --timeout=<ms>` per-test wall-clock cap (docs/future.md
> §3b item #4; the wall-clock sibling of `--max-heap`).** Opt-in per-test timeout: a `test fn` running
> longer than `N` ms is **hard-aborted** (un-recoverable — `recover:` CANNOT swallow it) and bucketed in
> a new `Verdict::TimedOut` (rendered `TIMED-OUT name (file) msg`, counts as failure, exit non-zero;
> summary appends `, T timed out` only when `T>0` so timeout-off output stays byte-identical). `0`/omitted
> = OFF, the default. **Mirrors the `--max-heap` arc one-to-one EXCEPT the observation site.** The abort
> stamps an **`is_timed_out` marker onto the `RuntimeError`** (mirrors `is_over_memory`/`is_assert`,
> excluded from `Display` so parity strings are byte-identical) and FORCES it back on after every unwind,
> so it crosses native-reentry `run_until` + the `spawn` worker→parent boundary; the `run_until` Err
> funnel bypasses `recover:` whenever it is set (EXTENDED the existing `is_over_memory` funnel arm, not a
> parallel path), and `verdict_from_fault` reads `e.is_timed_out` FIRST. **THE KEY DIFFERENCE (and the
> fix for a prior hang bug):** the deadline is observed at the **loop back-edge** in `jump_checked` — NOT
> at the M:N reds checkpoint. The reds checkpoint fires only for scheduler-dispatched fibers, but the
> top-level test body (`invoke_test → run_proto → run_until`) runs OUTSIDE the fiber scheduler, so a plain
> `test fn: while true: pass` would hang forever if gated only there. The back-edge runs
> engine-independently on every loop iteration and covers BOTH the top-level body AND `spawn`ed-task loops
> (a single check catches both). **Seam 1 (VM):** per-VM config `timeout_ms: u64` + `deadline:
> Option<Instant>` + `deadline_tick: u16` (NOT swapped by `swap_ctx`); `set_timeout`/`set_deadline`/
> `arm_deadline` beside `set_max_heap`/`reset_over_memory`; a fresh `now + timeout_ms` armed at each
> invoke entry (`invoke_test`/`invoke_suite_method`/`build_suite_instance`). **Seam 2 (jump_checked):**
> inside the existing `target < ip` back-edge branch, `if let Some(dl) = self.deadline` FIRST (zero clock
> reads when off — the hot-path invariant), throttled to one `Instant::now()` per 1024 back-edges via
> the wrapping `deadline_tick`; on trip returns `self.err(...).timed_out()`. **Seam 3 (spawn):**
> `spawn_worker` threads the SAME absolute `deadline` onto the M:N worker (`worker.set_deadline`) beside
> `set_max_heap`, so a spawned hang trips on the worker's own loop and the marker crosses back. **Seam 4
> (runner):** `Verdict::TimedOut{msg}`, render arm, summary count/clause, `run_tests_timed(root, parallel,
> max_heap, timeout_ms)` wrapper (`run_tests_capped` delegates `timeout_ms=0` → existing call sites
> byte-identical), threaded through `run_file` → `invoke_all` (`vm.set_timeout`), `verdicts()` gate parser
> learned the `TIMED-OUT ` prefix. **Seam 5 (CLI):** `cmd_test` parses `--timeout=<MS>` (u64; bad value →
> eprintln + FAILURE) + the **M:N-ENGINE-ONLY guard** (`--timeout` errors with `--serial` — a wall-clock
> trip is non-deterministic → no serial==M:N parity; the dual-engine gate runs timeout-OFF, untouched) +
> help line. VM + runner + `main.rs` only — **NO checker/compiler change.** Tests (M:N, robust to CI
> timing): `timed_out_bucket_for_infinite_loop` (THE regression test — top-level `while true: pass` →
> TIMED-OUT, proven RED-first: it hung under a `timeout 90` wrapper with the back-edge check disabled,
> exit 124), `timeout_control_passes_under_generous_timeout` (fast test / 60s cap → PASS, no clause),
> `recover_does_not_catch_timeout` (`recover:` can't swallow it), `timed_out_across_spawn` (spawned hang →
> TIMED-OUT). `chz_suite_passes_both_engines` green (runs timeout-off). **v1 limits (watchdog follow-up,
> §3b #4):** a test blocked in a **native call** (blocking syscall, `Channel.recv` with no traffic) or in
> **loop-free infinite recursion** (hits the stack guard) is NOT caught — a true watchdog thread is the
> next seam. Ms granularity; sub-ms overshoot. Verified end-to-end on the release binary (TIMED-OUT + exit
> 1; `--serial` guard error + exit 1; bad value + exit 1; timeout-off byte-identical PASS).
>
> **✅ TEST RUNNER (2026-07-24) — `chezzi test --max-heap=<bytes>` per-test memory cap (docs/future.md
> §3b item #1b; builds on the FAIL/ERROR bucket infra).** An opt-in runaway-allocation guard: a single
> test whose in-VM live heap exceeds `N` bytes is **hard-aborted** (un-recoverable — `recover:` CANNOT
> swallow it, so a `for: recover: <alloc>` loop can't defeat it) and bucketed in a new
> `Verdict::OverMemory` (rendered `OVER-MEMORY name (file) msg`, counts as failure, exit non-zero;
> summary appends `, M over-memory` only when `M>0` so cap-off output stays byte-identical). `0`/omitted
> = OFF, the default. **Deterministic-in-VM, NOT OS RSS** (RSS is process-global + non-deterministic →
> would break the gate): checked against `Heap::live_bytes()`, the same `lb` already computed once per
> `sweep()` — a **per-heap** high-water, so the cap-OFF dual-engine gate is untouched. **Seam 1
> (heap):** `Heap` gains `mem_cap: usize` (0=off) + `over_cap: bool`; `sweep()` sets `over_cap = mem_cap
> != 0 && lb > mem_cap` (reuses the peak-probe `lb`, no second scan) + `set_mem_cap`/`over_cap`/
> `clear_over_cap`. **Seam 2 (VM):** `run_until`'s post-`collect()` boundary hard-aborts via
> `unwind_deferred(base_level, false)` (report=false = bypass `recover:`, exactly like the
> `self.cancelled` funnel). The check is **re-observed at every GC boundary like a cancel checkpoint — NO
> latch** (an earlier `Vm::over_memory` re-fire latch was REMOVED — it disabled the cap for the abort's
> own cleanup unwind, so a runaway `defer` ran uncapped); a `defer` that itself allocates runaway is now
> bounded (its nested `run_until` re-trips), while a non-allocating defer runs to completion
> (`should_collect()` resets after each collect). `over_cap` is cleared per test at
> `invoke_test`/`invoke_suite_method`/`build_suite_instance`; `set_max_heap` threads the cap. The abort
> stamps an **`is_over_memory` marker onto the `RuntimeError`** (mirrors `is_assert`, excluded from
> `Display`) and FORCES it back on after every unwind, so it travels WITH the error across an enclosing
> native-reentry `run_until` (HOF callback / operator overload / deferred call) AND a `spawn`'d worker's
> fault crossing back to the parent — the `run_until` Err funnel bypasses `recover:` whenever the marker
> is set. `spawn`/`parallel:` runaway tasks are covered on M:N too: `spawn_worker` threads `mem_cap` onto
> the worker's own heap. **Seam 3 (runner):** `Verdict::OverMemory{msg}`, `verdict_from_fault(e)` checks
> `e.is_over_memory` FIRST, `run_tests_capped(root, parallel, max_heap)` wrapper (2-arg `run_tests`
> delegates → the ~14 existing call sites stay byte-identical), `verdicts()` gate parser learned the
> `OVER-MEMORY ` prefix. **Seam 4 (CLI):** `cmd_test` parses `--max-heap=<N>` (plain bytes; bad value →
> eprintln + FAILURE). VM + runner + `main.rs` only — **NO checker/compiler change.** Tests: 2 heap unit
> (`mem_cap_off_never_trips`/`mem_cap_trips_when_live_exceeds`) + 7 runner
> (`over_memory_bucket_for_runaway_alloc` on BOTH engines, `over_memory_control_passes_under_generous_cap`,
> `over_memory_concurrent_under_cap_passes_on_both_engines`, `recover_does_not_catch_over_memory` + the
> native-reentry variant `recover_does_not_catch_over_memory_via_native_reentry`,
> `over_memory_buckets_across_spawn_on_both_engines`, and `over_memory_defer_is_still_capped_during_unwind`
> — the hard-abort + parity + defer-bounding proofs); `chz_suite_passes_both_engines` green (runs cap-off).
> **Guarantee + v1 limits (deterministic, documented in §3b #1b):** the cap is **per-heap** — any single
> execution context whose live heap exceeds `N` is aborted, so a real runaway trips on whichever worker
> heap runs it. **`--max-heap` is M:N-ENGINE-ONLY** (errors if combined with `--serial`) — this avoids a
> serial≠M:N divergence by construction: `--serial` shares one heap (measures `parent-baseline + Σ tasks`),
> M:N isolates each worker (measures a task alone), so a *concurrent* test near the boundary (allocation
> split below `N` per-fiber but summing above) would bucket `OVER-MEMORY` on serial yet pass on M:N. A
> cross-engine aggregate needs non-deterministic global RSS (rejected — would break the gate), so rather
> than ship the divergence the flag is restricted to the default engine (`--serial` is the parity oracle,
> slated for post-freeze removal). The trip also fires only at a GC boundary + on
> `Obj`-count growth — plus, since 2026-07-27, on charged off-heap wire bytes whenever a cap is set
> (a loop growing a container of inline scalars allocates no `Obj`s and charges no wire bytes, so it
> still never sweeps → never trips — push a heap value to guard it), and overshoots ~2×N before firing
> (`next_gc = 2*live`). k/m/g suffixes and
> `chezzi run --max-heap` deliberately out of scope (`--timeout` has since landed — see the entry above).
> Verified end-to-end on the release binary both ways
> (OVER-MEMORY + exit 1; bad value + exit 1; cap-off byte-identical PASS).
>
> **✅ TEST RUNNER (2026-07-24) — `chezzi test` FAIL vs ERROR split (docs/future.md §3b item #1, the
> foundation wave).** A `test fn`/method is void, so `assert` is the ONLY intended failure signal; every
> other runtime fault (OOB, div-by-zero, missing key, native fault, a crashed setup hook) is unexpected
> and now renders **ERROR**, not FAIL — pytest's FAILED-vs-ERROR distinction. **Seam 1 (VM):**
> `RuntimeError` (`src/vm/mod.rs`) gains `pub is_assert: bool` (default `false` via `#[derive(Default)]`;
> `Display` unchanged → parity strings byte-identical, verified no whole-struct `==` compares it), set
> `true` ONLY by the `Op::Assert` arm (`src/vm/exec.rs`). **Seam 2 (runner):** `Outcome.failure:
> Option<(usize,String)>` → an extensible `Verdict` enum `{ Pass, Fail{line,msg}, Error{line,msg} }`
> (`src/test_runner.rs`); a test-body fault routes assert→`Fail` else→`Error` (via `verdict_from_fault`),
> every setup/teardown fault (construction, `before_all`/`before_each`/`after_each`) is `Error`-class
> regardless of `is_assert`. Summary is now `P passed, F failed, E errored` (+ optional `K file
> error(s)`); `report.passed` requires `F==E==file_errors==0`; exit non-zero if any. The `verdicts()`
> dual-engine gate parser learned the `ERROR ` prefix so an errored test participates in serial==M:N.
> The `Verdict` enum is the extension point for the ergonomics wave's `TimedOut`/`OverMemory` buckets.
> Tests: 4 new runner `#[cfg(test)]` (error-bucket / fail-bucket / passing / hook-is-error); existing 9
> runner tests + `chz_suite_passes_both_engines` green; verified end-to-end on both engines (one FAIL +
> one ERROR + one PASS, identical, exit 1). **NO checker/compiler change.**
>
> **✅ CLEANUP (2026-07-23) — code-review dedup + doc-freshness (behavior-preserving, −202 LOC).** A 5-domain
> review (no correctness bugs) drove two batches: (A) rewrote ~69 src + ~50 docs/examples comments referencing
> the **removed** tree-walk interpreter to the real serial-VM vs M:N-VM parity story, plus doc-freshness fixes
> (test count 1500→3681, PROGRESS "Current focus", Tier-D heading, benchmarks `src/interp/*` dead paths, lexer
> tutorial scaffolding); (B) extracted verified-identical helpers — compiler `compile_args` (29 sites) +
> `emit_call_static` (6) + `is_unbound` (15), checker `check_type_arity_and_bounds` (struct/enum/newtype),
> native `unary_float!` macro (14 math fns) + stdlib `write!("{:02x}")` hex, VM `core_accessor!` macro (6) +
> `depth_exceeded_err` (5), and 4 dead-code inlines. 3681 lib + difftest/check_parity/conformance green, clippy clean.
>
> **✅ BUG-HUNT + FEATURE (2026-07-23, wave 3) — `Channel.trip()` type-hole FIXED via new scalar `where`-bounds.**
> `trip()` was typed `-> T` on every `Channel[T]` but always delivers `bool true` → a `bool` leaked through a
> `T`-typed `recv()` on any `Channel[int]`/etc. (check-OK type-soundness hole). Fix adds a declarative facet:
> `where T: <scalar>` is now an EQUALITY bound (int/float/bool/str/bytes/bytearray/nil) — `trip()` carries
> `where T: bool` in `std/prelude.chz`. Checker-only + additive (`scalar_bound_ty`, `satisfies_args_d`/
> `check_bounds` arms, Channel arm now `enforce_bounds`). 3681 lib tests + conformance green, clippy clean.
>
> **✅ BUG-HUNT (2026-07-23, wave 3) — native `str.count("")` / `str.split("")` empty-arg fixes.** Two pure-
> runtime native-method fixes in `src/vm/call.rs` (shared-wrong, so the serial==M:N oracle is blind — caught by
> Python/Go comparison). `"abc".count("")` returned `0`, now `4` (codepoint len + 1, matching Python/Go and the
> already-fixed sibling `std.string.count`; `5a8fba0` missed the native method). `"abc".split("")` leaked Rust's
> empty-pattern edges (`["","a","b","c",""]`), now raises a recoverable `split: sep must not be empty` fault
> (matching `std.string.split`). Tests: `str_count_empty_sub` + `str_split_empty_sep_faults` in
> `reserved_method_tables_test.chz`.
>
> **✅ FIXED (2026-07-23) — cyclic-key equality inconsistency (bug-hunt wave-3 finding #4).** Container
> membership / key-equality (`Set.has`/`add`, `Map` get/insert/remove, `in`, set algebra `\| & - ^`,
> `List.contains`/`index_of`/`unique`/`dedup`, Atomic `cas`) SWALLOWED the recoverable `"maximum structural
> depth (10000) exceeded"` fault that `==` correctly raises on a genuinely cyclic key — silently returning a
> wrong `false` instead. Root cause: the `Vm::values_equal` wrapper (`arith.rs`) `unwrap_or(false)`'d the
> guarded worker's depth `Err`. Fix (pure runtime, ~25 sites): three `#[inline]` `?`-propagating helpers
> `seq_slot`/`set_slot`/`map_slot` + inline `?`-loops replace the swallowing `.any`/`.find`/`.position`
> closures; `set_op` grew a `span`+`Result`; the wrapper is now `#[cfg(test)]`-only. Cyclic keys now fault
> RECOVERABLY (byte-identical to `==` / Python `RecursionError`) on both engines; non-cyclic behavior is
> unchanged. Also: `chezzi test`'s SERIAL pass now runs on `on_vm_stack` (was inline on the 8 MB main thread
> → a 10000-deep walk `SIGABRT`ed only there; M:N already had the 384 MB VM stack). Tests:
> `cyclic_key_faults_everywhere` + `noncyclic_controls` in `tests/chz/spec/map_set_test.chz`.
>
> **✅ CHECKER REFACTOR (2026-07-23) — the last 4 bespoke reserved-type method tables are now file-backed.**
> `channel_method_sig`/`str_method_sig`/`bytes_method_sig`/`bytearray_method_sig` (`src/checker/mod.rs`) are
> RETIRED: each is now a body-less `native struct Channel[T]`/`str`/`bytes`/`bytearray` in `std/prelude.chz`,
> harvested into the reserved type's method table (`container_seeds` → `seed_stdlib_structs`) and resolved via
> `native_handle_method` (scalars pass no type args → the identity path returns the stored sig verbatim) — the
> exact List/Map/Set/Shared/Socket/Writer pattern. Checker-only, sigs BYTE-MATCH the retired arms (incl.
> `str.message` for the Error protocol, `to_int`→`Option[int]`, `parse_int`→`Result[int, str]`,
> `split`→`List[str]`). Runtime dispatch UNTOUCHED (`vm/netio.rs channel_method` / `vm/mod.rs core_method` /
> `bytes_method`). The `bytearray.extend` special-case (accepts bytes|bytearray|List[int]) stays an explicit
> branch BEFORE the table lookup. `unique_member_owner`'s `str`/`bytes` pin now reads the seeded tables
> (`src/checker/pattern.rs`). Every reserved-type method sig is now file-backed — no bespoke `*_method_sig`
> fn remains. Tests: `tests/chz/spec/reserved_method_tables_test.chz` (10 tests, both engines) + extended
> `prelude_container_checker`/`builtin_method_slices_all_resolve`. Docs: `docs/gaps.md` item 1 (mirror-gap half
> closed; VM `handle_key` half still out of scope), `docs/concurrency.md`.

> **✅ CHECKER DIAGNOSTIC (2026-07-23) — imported native-struct redeclare now says "already defined".**
> `import Match from std.regex` + `struct Match` reported "type 'Match' is reserved (builtin)" — wrong:
> `Match`/`Response`/`ProcResult`/`FileInfo`/`Ref` are first-class Rust-bridged module-exported types,
> NOT reserved (a bare unimported `struct Match` is legal). It is an ordinary import-name collision, so
> it now reads "type 'Match' is already defined" — aligned with the enum/newtype/typealias sibling arms
> (which already said so via `struct_names`). Fix: struct hoist-guard (`src/checker/setup.rs:~2337`)
> moved `imported_builtin_types.contains(name)` out of the reserved branch into `already_defined`. Still
> a hard reject (no accept-then-trap); message-only. Genuine global reserved types (`int`/`Channel`/…)
> keep "reserved (builtin)". Tests: updated `import_plus_same_name_struct_decl_rejected` + 3 regressions.
> Docs: `docs/gaps.md` (note RESOLVED-as-reframed), `docs/stdlib.md`.

> **✅ AIRLOCK (2026-07-23) — value-store paths now handle-gated (serial==M:N).** An FFI/native/module
> handle crossing `Channel.send`/`try_send`/`wait:`-send or stored into a `Shared`/`RwShared`/`Atomic`
> (construct/set/update/store/exchange/CAS) was UNGUARDED — bare `to_wire_at`, no `ensure_crossable`. On
> `--serial` the handle crossed silently (and even executed); on M:N `from_wire` rebuilt a garbage
> cross-heap `GcRef` → serial≠M:N + type confusion. Root-cause fix: one `Vm::to_wire_crossable` helper
> (`= to_wire_at` then `ensure_crossable`, `src/vm/sched.rs`) swapped in at every value-store site
> (`src/vm/netio.rs`, `src/vm/exec.rs`) so a NEW store path physically can't forget the guard. Both
> engines now reject identically + recoverably (`recover:` catches it) at the send/store/construction
> site with the `a module handle cannot cross` message. Legit `Channel`/`Shared`/
> `Executor`/socket handles (shared-`Arc` wire arms, `has_handle()`==false) still cross unchanged —
> regressed by `positive_*` parity tests. VM-only; checker/compiler untouched. Tests: 4 fault + 2
> positive in `vm/parity_tests.rs`. Docs: `docs/gaps.md` L7 (the "caught at the runtime airlock" claim
> was false for value-store paths — now noted CLOSED). (**Update 2026-07-23:** the 4 FFI-fault tests
> were later FLIPPED to positive — native/FFI fn VALUES now cross by value; see next entry.)

> **✅ FIX (2026-07-23, `auto-task/native-ffi-wire-airlock`) — native (`Obj::Native`, e.g. `math.sqrt`)
> + FFI (`Obj::Cffi`, `extern "lib":`) fn VALUES now cross the wire-value airlock BY VALUE.** They passed
> `chezzi check` (type `Ty::Func`, checker-sendable) but FAULTED at runtime on the wire path
> (`Channel.send`/`Shared`/`Atomic`/`RwShared`/`Executor.submit`/`spawn use(f)`) while the SAME value
> crossed FINE via the snapshot path (a `spawn:` block capturing it) — a pure internal inconsistency.
> Root cause: `to_wire_depth` (`src/vm/sched.rs`) lumped the two pure-code arms (`Native`, `Cffi`) with
> `Module` into `WireValue::Handle(h)` (a raw `GcRef` meaningless on another heap → `has_handle()` →
> reject). Fix mirrors the shipping `Builtin` + `SnapValue::Native`/`Cffi` template: new by-value/by-`Arc`
> `WireValue::Native { name, func }` + `WireValue::Cffi(Arc<Cffi>)` arms, a split `to_wire_depth` arm
> (`Module` stays `Handle`; Native→fn-ptr by value, Cffi→shared `Arc`), `from_wire` rebuild arms next to
> `Builtin`, and `collect_core_gcrefs`/`display_wire` arms. `ensure_crossable` diagnostic corrected to
> `a module handle cannot cross` (Module is source-unreachable → defensive-only). Serial == M:N
> byte-identical (native + FFI via Channel/Shared/spawn-arg/spawn-block). VM-only; checker/compiler/parser
> untouched (the checker was already correct). Tests: `tests/chz/spec/airlock_native_test.chz` (4 native)
> + 5 flipped `ffi_handle_crosses_*`/`ffi_handle_send_succeeds` parity tests. Docs: `docs/gaps.md`
> session log + settled-note split.

> **✅ FIX (2026-07-23, `auto-task/newtype-sort-minmax`) — numeric-newtype `.sort()`/`.min()`/`.max()`
> now honor `Comparable` at runtime (checker⊋compiler soundness class).** A numeric `newtype`
> (`= int`/`= float`) satisfies `Comparable`, so `check` accepted `List[newtype].sort()`/`.min()`/`.max()`
> — but the runtime comparators never unwrapped the `Obj::NewType` box: `Vm::value_order` fell to
> `_ => Equal` (sort **silently no-op'd**) and `Vm::compare` returned `None` (min/max **faulted** with
> *"sort_by_key keys are not comparable"*). Both engines agreed on the broken behavior → parity-blind.
> Fixed with a newtype-unwrap-and-recurse arm at the top of both comparators (`src/vm/arith.rs`) — orders
> by the underlying's *native* scalar order, matching bare `<` (`compare_op`) and the checker grant.
> Checker untouched (its grant was correct). Regression: `tests/chz/spec/newtype_test.chz` (int + float
> sort/min/max + bare `<`/`==` + `sort_by_key` positive controls), gated serial==M:N. A `str`/`bool`
> newtype is not `Comparable` (checker grants numeric only), so its sort is rejected at `check`. See
> `docs/gaps.md` session log 2026-07-23.
>
> **↳ Follow-up (2026-07-23, `auto-task/order-key-newtype`) — `Vm::order_key` was the MISSED sibling.**
> The above fix covered `.sort()` (`value_order`) but the `.min()`/`.max()`/`.min_by`/`.max_by`/`.sort_by_key`
> path actually routes through a *separate* comparator, `Vm::order_key` (`src/vm/call.rs`), which was still
> un-unwrapped — so a `List[newtype=float]` key holding a `math.nan` faulted *"sort_by_key keys are not
> comparable"* at `.min()`. Mirrored the same newtype-unwrap-and-recurse arms into `order_key`; also closes a
> benign `-0.0`/`+0.0` `min`-vs-`sort` inconsistency (`order_key` now uses `total_cmp` like `sort()`). Checker/
> `value_order`/`compare` untouched. Regression: `minmax_nan_float_newtype` + `by_key_nan_float_newtype`.

> **✅ TESTING (2026-07-23) — dedicated native test suite `tests/chz/` + dual-engine parity gate.**
> New home for Chezzi-language behavior tests, separate from `examples/` (print-and-golden demos):
> `tests/chz/{spec,stdlib,suites}/` with a `README.md`. The 10 existing `examples/*_test.chz` MOVED to
> `tests/chz/suites/` (golden-twin refs in their headers preserved). **The linchpin:** `chezzi test`
> runs a SINGLE engine, so porting Rust `parity_*` tests would drop the serial==M:N dimension. Fixed by
> teaching the runner a `parallel` flag (`test_runner::run_tests(path, parallel)` → new `invoke_all`;
> M:N runs on `crate::vm::on_vm_stack` + `Vm::set_parallel`) and a `cargo test` gate
> `chz_suite_passes_both_engines` that runs the whole `tests/chz/` suite on BOTH engines and asserts
> identical per-test verdicts (verdicts parsed from the report). Verified real: breaking one `.chz`
> assert fails the gate with `file:line`. **`chezzi test` CLI defaults to the M:N engine** (matches
> `chezzi run`, forward-compatible with the post-freeze serial removal); `--serial` opts into the
> cooperative VM (`--serial`/`--parallel` mutually exclusive, `--parallel` a no-op alias). **Ported clusters (4):** `spec/list_test.chz` (15),
> `spec/map_set_test.chz` (10), `spec/conversions_test.chz` (6), `stdlib/math_test.chz` (5) — from
> `vm/tests.rs` (`list_*_parity`, map/set comprehensions + equality) and `parity_tests.rs`
> (`parity_list_*`, `parity_map_*`, `parity_set_struct_algebra`, `scalar_ctor_conversions_parity`,
> `bool_ctor_parity`, `math_number_fns_parity`). **38 fully-covered Rust test fns DELETED** (~370 net
> Rust lines removed; the ~108-line harness is a one-time cost, so each further cluster is pure
> reduction). KEPT in Rust (no in-language equivalent): fault-path (`assert` can't catch a fault
> message), GC-stress (`run_capture_stress` rooting), unannotated-closure inference
> (`list_hof_3engine_parity`), missing-hash runtime-bypass. Follow-up: iterate remaining clusters (str
> methods, tuples, encoding/crypto → `tests/chz/stdlib/`). Drive-by: unstuck a pre-existing
> stale golden `examples/str_more.expected` (`count("")`=len+1 from the prior commit, golden never
> updated). Docs: `docs/syntax.md §9c`, `tests/chz/README.md`.

> **✅ CONCURRENCY (2026-07-24, `auto-task/rwshared-readview`) — zero-copy READ-view methods on
> `RwShared[List[E]]`.** Purely ADDITIVE; `get`/`read`/`write`/`set`/`update` + the `WireValue` storage
> model untouched; `Shared`/`Atomic`/`Executor` untouched. Five methods: `len() -> int`,
> `at(i) -> E` (bounds-checked, negative index; OOB = recoverable fault), `slice(lo, hi) -> List[E]`,
> `for_each(f: fn(E) -> _) -> nil`, `fold(init: R, f: fn(R, E) -> R) -> R` (R inferred from `init` via the
> `List.fold[U]` generic route through `infer_generic_method` — NOT the `read` R-recovery hack). **The
> burden fix:** `get`/`read` `from_wire`-materialize the WHOLE stored inner into the caller's heap on every
> access (fan-out of a 1M-int list to 8 workers ≈ 1587 MB); the read-view walks the heap-independent
> `WireValue::List` Vec UNDER the read guard and `from_wire`s ONE element at a time → O(1) memory for a
> reduce. Checker: arm-only sigs at the `Ty::RwShared(elem)` arm (`src/checker/expr.rs`), gated to
> `elem = List(E)` — a non-list `T` (`RwShared[int].fold`) is a clean "no method" reject (no
> check-OK-then-run-fault). Runtime: five arms on `rwshared_method` (`src/vm/netio.rs`) mirroring the
> list-HOF accumulator rooting. **Guard-lifetime (fixed after adversarial review — the guard is NEVER held
> across user code):** `for_each`/`fold` RE-ACQUIRE the shared `core.v.read()` guard PER ELEMENT, clone one
> wire element, and DROP the guard before running the callback (mirroring `read`'s clone-out-then-drop, per
> element). The original impl held one read guard across the whole walk — which deadlocked on M:N against
> the write-preferring `std::sync::RwLock` in three ways a concurrent writer could trigger (a callback's
> nested read of the SAME box; an AB-BA cross-box walk; the GC's mark of `Obj::RwShared`, which re-locks
> `core.v`). Per-element re-lock removes all three: still O(1) memory (one element materialized per step),
> deadlock-free (nested read AND write of the same box now work), at the cost of the walk NOT being one
> atomic snapshot (a concurrent/in-callback `set` may be seen mid-walk — use `read`/`get` for a stable
> snapshot; index re-checked each step so a shrinking list can't panic). serial == M:N byte-identical
> (walks a heap-independent wire form) — proven by
> `tests/chz/suites/rwshared_readview_test.chz` (both engines via `chz_suite_passes_both_engines`, incl. a
> nested-read-under-concurrent-writer stress case that deadlocked the pre-fix impl) + checker `rejects`/`ok`
> pair. Extended to `Map`/`Set` — see the entry above. Docs: `std/concurrency.chz`, `docs/stdlib.md`,
> `docs/concurrency.md §6f`.

> **✅ CONCURRENCY (2026-07-24, `auto-task/rwshared-map-set-readview`) — zero-copy READ-view extended to
> `RwShared[Map[K,V]]` + `RwShared[Set[E]]`** (Tuple EXCLUDED — heterogeneous). ADDITIVE; storage model +
> `get`/`read`/`write`/`set` + `Shared`/`Atomic`/`Executor` untouched. New methods: **Map** `len()`,
> `get_key(k) -> Option[V]`, `has(k) -> bool`, `for_each_entry(f: fn(K,V) -> _) -> nil`,
> `fold_entries(init: R, f: fn(R,K,V) -> R) -> R`; **Set** `len()`, `contains(e) -> bool`,
> `for_each(f: fn(E) -> _) -> nil`, `fold(init: R, f: fn(R,E) -> R) -> R`. **Gating migrated to a
> constructor-kind `where T: List/Map/Set` bound** (`container_bound_matches` in `src/checker/proto.rs`,
> the generalization of `scalar_bound_ty`: head-constructor equality, no element binder — so no
> harvest-scoping change). The checker arm (`src/checker/expr.rs`) now branches on the container HEAD
> first, then the method within that container's set (names OVERLAP — `len` on all three, `for_each`/`fold`
> on List+Set), arm-recovering E/K/V by destructuring the concrete `List[?E]`/`Map[?K,?V]`/`Set[?E]`; a
> wrong head OR a wrong method for the head falls through to a clean "no method" reject (no
> check-OK-then-run-fault; `RwShared[Map].fold`, `RwShared[int].contains`, `RwShared[Tuple].len` all
> reject). `fold*`'s R pins from the concrete `init` via `infer_generic_method`. Runtime
> (`src/vm/netio.rs`): `len`/`for_each`/`fold` arms extended to walk `WireValue::Map`/`Set`; new
> `get_key`/`has`/`contains` hash the query key ONCE (guard NOT held), then LINEAR probe RE-LOCKING per
> entry — compare the cached wire hash under the guard, and only on a hash-match clone the entry, DROP the
> guard, `from_wire`, `values_equal_guarded` (collisions keep scanning; the guard is NEVER held across
> hash/eq/closure — same deadlock invariant as List). serial == M:N byte-identical (heap-independent wire
> walk) — proven by the extended `tests/chz/suites/rwshared_readview_test.chz` (both engines via
> `chz_suite_passes_both_engines`, incl. Map+Set nested-read-under-writer + AB-BA cross-box deadlock
> regressions) + checker `rejects`/`ok` boundary tests (`container_where_bound_*`, `rwshared_map/set_*`).
> The `where T: List/Map/Set` bound is now a genuine, tested surface bound (a user generic `fn f[T: List]`
> accepts a list, rejects an int). Docs: `std/concurrency.chz`, `docs/stdlib.md`, `docs/concurrency.md §6f`.

> **✅ CONCURRENCY (2026-07-22, `auto-task/atomic-int`) — `AtomicInt`, a monomorphic LOCK-FREE int atomic.**
> Purely ADDITIVE; `Atomic[T]` untouched (zero regression). The reframed backlog item from `docs/future.md
> §4` (a lock-free fast path on the GENERIC `Atomic[T]` was UNSOUND — VM is type-blind at construction, so
> `Atomic[Any]` holding an int then `.store("hi")` faults; a monomorphic type removes the cause). Shipped
> mirroring the four reserved `std.concurrency` names at every checker/VM site as a UNIT type (no `[T]`):
> `Ty::AtomicInt` + `Obj::AtomicInt(Arc<AtomicIntCore{v: AtomicI64}>)` + `Op::NewAtomicInt` +
> `WireValue::AtomicInt`; `native struct AtomicInt` (no `[T]`, concrete int sigs) in `std/concurrency.chz`
> harvests the method table. Methods `load/store/exchange/cas/add/sub` all int, `add`/`sub` ALWAYS valid
> (int is always numeric — no residual gate). **The one piece of real logic:** `add`/`sub` use a CHECKED
> `compare_exchange` CAS-loop (NOT raw `fetch_add`/`fetch_sub`, which wrap silently) — keeps the
> i64-overflow fault byte-identical to `Atomic` (`"integer overflow in Add/Sub"`); `SeqCst` on every op →
> serial == M:N byte-identical. Import-gated + reserved like `Atomic` (bind_import skip closes the
> reserved-name hole). **Perf: ~2.7× faster than Mutex-backed `Atomic` on an 8-way contended int counter**
> (16M adds, 1.73s vs 4.73s; uncontended a wash) — `docs/benchmarks.md §AtomicInt`. Tests (both engines):
> `atomic_int_{roundtrip,add_overflow,sub_overflow,contention,from_import_runs}_parity` +
> `atomic_int_bare_unlicensed_errors` + harvest-drift/reserved-name loops + `examples/atomic_int.chz`.
> Docs: `future.md` (LANDED), `stdlib.md`, `concurrency.md §6b`, `syntax.md`, `benchmarks.md`.

> **✅ CONCURRENCY (2026-07-22, `auto-task/wait-send-arms`) — `wait:` SEND-arms (Go-`select` symmetry).**
> A `wait:` arm can now be a bare `ch.send(v):` (no `:=`/`=`), the send-side twin of `x := ch.recv():`.
> Ready when the channel can accept the value — **bounded-with-space** / **unbounded** (always) / **closed**
> (selected + faults `"send on a closed channel"`, NOT skipped like a closed recv-arm; Go's panic-on-send-
> to-closed). **Selection stays deterministic SOURCE ORDER** (first ready arm wins, recv OR send) — the one
> principled divergence from Go's random fairness, and what keeps serial == M:N byte-identical. All arm
> handles + send values are evaluated once, top-to-bottom, on entry (Go's rule). *AST reshape:*
> `WaitArm{target,chan,body}` → `WaitArm{kind: WaitArmKind::{Recv{target,chan}|Send{call}}, body}` (Option C —
> the checker owns send-shape validation; the parser is lenient, any bare `<expr> <block>` → `Send{call}`).
> *Checker:* a bare arm must be exactly `chan.send(value)` (`chan: Channel[T]`, `value: T`) — else the
> legal-forms error; a valid shape reuses the ordinary `ch.send(v)` type-check. *Compiler/VM:* a send-arm
> leaves TWO operand slots (chan, value) vs a recv-arm's one; `WaitMeta.is_send` drives a per-arm slot cursor
> in `op_wait_poll`; `take_wait_send_arm` commits (no value pushed). *Scheduler (the delicate bit):* the M:N
> `park_wait` gap re-check is now **kind-aware** — a send-arm is ready with a FREE slot (`queue.len()<cap` or
> unbounded) / on close, a recv-arm with a queued value / on close; the **wake side is unchanged** (a receiver
> freeing a slot already calls `wake_senders`→`recv_wake`, which wakes the filed `WaitPark` token). A full
> bounded send-arm reached inside a native callback **faults** on **both** engines with byte-identical text
> (v1 limit; the fault is decided before the engine split, mirroring the existing in-callback full-`send`
> demote fault; upgrade path noted). *Checker soundness:* a send-arm's receiver is verified to be a
> `Channel[T]` (a user type that merely HAS a `send` method is rejected — else the compiler lowers it as a
> channel op and `op_wait_poll`→`channel_core` `unreachable!`-panics). *Tests:* `examples/wait_send.chz` +
> `golden_wait_send_both_engines`, `wait_send_arm_park_wake_stress_parallel` (40 M:N trials),
> `wait_send_arm_closed_channel_faults_both_engines`, `wait_unbounded_send_arm_always_ready_both_engines`,
> `wait_send_source_order_first_ready_wins_both_engines`, `wait_send_arm_in_callback_faults_same_on_both_engines`;
> checker `wait_send_arm_*` (incl. `wait_send_arm_non_channel_receiver_rejected`) / `wait_bare_*`; parser
> `parses_wait_bare_send_arm`. Docs: this note, `docs/grammar.bnf` (new `<waitArm>` bare-expr form),
> `docs/concurrency.md §6d`, `docs/syntax.md`. `cargo test` / `conformance` / `clippy` green.

> **✅ REFACTOR (2026-07-22, `auto-task/unify-native-dispatch-prefix`) — UNIFIED native-handle dispatch
> prefix (checker + VM), behavior-preserving.** Kills the check-OK/run-fault bug class STRUCTURALLY (the
> one just hit where `try_native_bodied_method` was missing on the `Shared`/`RwShared`/`Atomic`/`Executor`
> VM arms → type-checked then runtime-faulted). *VM (`src/vm/call.rs`, `do_method_call`):* the eight
> per-handle `if matches!(Obj::X) { try_native_bodied_method("X", …); x_method(…) }` arms
> (`Shared`/`RwShared`/`Atomic`/`Executor`/`Socket`/`Listener`/`Writer`/`Reader`) collapse into ONE
> `match self.heap.get(h) → Some("<key>")` — a handle runs the bodied-method probe ONCE then dispatches
> its native op body (tails byte-identical: Socket/Listener `poll_park`, Writer `stream_halt`); a `None`
> falls straight to the hot collection arms. Adding a handle to that one match auto-enables bodied
> dispatch — no arm can be forgotten. Bonus: this REMOVES the eight `if matches!` probes that used to sit
> on the hot list/map/struct method path (net fewer branches; benches flat, `docs/benchmarks.md`).
> *Checker (`src/checker/expr.rs`, `infer_method_call`):* the shared lookup + hover + generic-branch
> (`native_handle_method` → `record_method_hover` → prepend-receiver `infer_generic_method`) extracted
> into `resolve_native_handle_method` (returns `Generic(Ty)`/`Concrete(FnSig)`/`Miss`); each arm keeps
> its residual special case INLINE — Atomic numeric gate (BEFORE lookup), Executor `submit` capture-floor,
> RwShared `read` R-recovery; the four fixed net/io handles share `infer_fixed_native_handle_method`, now
> structurally routed through the generic branch (a proven no-op today, forward-compatible for a future
> bodied generic method). EXCLUDES List/Map/Set entirely (hot `core_method` arm untouched — M19). No
> stdlib/behavior change: proven by the two existing bodied methods flowing through the unified prefix
> (`Executor.submit_result[T]` generic + `Reader.lines` non-generic, both engines) plus every existing
> special-case/parity/golden test staying green. Tests: unchanged (behavior-preserving); full `--lib`
> (3688) + all targets + conformance green, clippy clean. Docs: this note, `docs/benchmarks.md`.

> **✅ CONCURRENCY (2026-07-22, `auto-task/bounded-channel-pmap`) — BOUNDED `Channel[T](cap)` +
> `pmap`/`pmap_limited` stdlib helpers.** *(1) Bounded channel:* `Channel[T]()` stays unbounded (`send`
> never blocks — byte-identical to before); `Channel[T](cap)` (`cap > 0`; `cap <= 0` is a runtime fault
> `"Channel capacity must be > 0"`) is a bounded FIFO whose `send` **blocks/parks** while `cap` messages
> are queued and resumes on a `recv` freeing a slot (Go's buffered channel). New surface: `cap() -> int`
> (0 for unbounded); `try_send` now returns `false` on **full OR closed** (was closed-only). Only
> `send`/`try_send` changed — `recv`/`try_recv`/`close`/`for`-drain/`trip`/`len` untouched. The send-park
> is the send-side twin of the recv park: a new `send_suspend` sentinel + `Disp::SendPark` +
> `MnSched::park_send` (gap re-check = *space available*, the opposite of `park`'s *message waiting*) +
> `send_wake_bounded` (atomic space-check+enqueue+wake under the sched lock, no over-cap race) +
> `recv_wake`/`Vm::wake_senders` called after every bounded pop (`recv`/`for`-drain/`try_recv`/`wait:`/
> demote). A parked sender is filed as an ordinary `ParkedEntry::Recv` — the bucket is homogeneous
> per-instant (a `cap>=1` channel is never simultaneously full and empty), so `is_deadlocked` is
> UNCHANGED and a full-send with no consumer (top level / native callback) faults with one shared
> `FULL_SEND_DEADLOCK` string (identical text both engines → parity). Parity holds by the blocking-`recv`
> argument: backpressure changes *which* task runs *when*, never the value sequence a consumer sees.
> **Deferred (noted, not built):** ~~send-arms in `wait:`~~ **LANDED 2026-07-22** (see the wait-send-arms
> entry above), and a demote-in-place bounded send inside a native callback (`ponytail:` upgrade path; v1
> faults — still deferred, and now also the send-arm-in-callback fault). *(2) `pmap`/`pmap_limited`:* pure-Chezzi scoped
> parallel-map helpers in `std/concurrency/pmap.chz` (results in **submission order** via sort-by-index,
> never completion order; `pmap_limited` caps in-flight tasks with a channel-as-semaphore token bucket).
> Tests: `src/vm/tests.rs` `bounded_channel_*` (cap/try_send-full/zero-cap-fault/full-send-deadlock/
> fan-out golden, all serial==M:N) + `pmap_*`; checker `channel_bounded_capacity_*`. Docs: this note,
> `docs/concurrency.md` §5/§6d, `docs/stdlib.md`, `docs/spec.md`.

> **✅ CHECKER + VM (2026-07-22, `auto-task/generic-native-methods`) — generic methods on RESERVED
> built-in receivers, 3 mirror-edits.** `docs/gaps.md` "Generic methods on RESERVED built-in receiver
> types" **1a/1b/2 RESOLVED**: (1a) `method_has_own_type_params` (`src/checker/expr.rs`) gained
> reserved-receiver arms (bare-table lookup like `Ty::Struct`), so member turbofish `[1,2,3].map[int](…)`
> and any bodied generic method are no longer rejected "takes no type argument(s)"; (1b) the
> `Ty::Shared`/`RwShared`/`Atomic`/`Executor` arms now route a harvested method carrying `type_params`
> through `infer_generic_method` (prepend the concrete receiver — verbatim mirror of the `Ty::List` arm),
> so a bodied `fn m[U](self,f:fn()->U)->U` opens `[U]` and infers from the closure; (2)
> `try_native_bodied_method` is now wired into those 4 arms of `do_method_call` (`src/vm/call.rs`),
> mirroring `Writer`/`Reader` — closes the check-OK/run-fault gap. **Shipped proof:**
> `Executor.submit_result[T](f: fn() -> T) -> Channel[T]` (`std/concurrency.chz`) — the FIRST bodied
> generic method on a native struct; `submit_task` (`std/concurrency/task.chz`) now builds over it
> (semantically identical). Tests: `vm::tests::executor_submit_result_both_engines`, checker
> `reserved_receiver_generic_method_turbofish_ok` / `reserved_receiver_nongeneric_method_turbofish_rejected`
> / `executor_bodied_generic_method_infers_from_closure`; the two existing `task_*` tests stay green. Docs:
> this note, `docs/gaps.md`, `docs/stdlib.md`, `docs/concurrency.md`. **Residual (by design):** list/map/set
> bodied methods stay unharvested (hot `core_method` arm untouched — M19 perf); `ex.submit_task(f)` dot-form
> still needs the deferred Task-placement change (Option A).

> **✅ CLI (2026-07-22) — `chezzi run --check-parity <file>`: run the parity oracle yourself.** The
> test-only serial==M:N oracle (`assert_file_parity`) is now a one-command user check. `--check-parity`
> type-checks once, then runs the program TWICE — the cooperative serial oracle (`parallel=false`),
> then the M:N engine (`parallel=true`) — each into a BUFFERED sink (`run_file_with_entry` with the
> default `stream=false`), and diffs stdout/stderr/rendered-terminal-error byte-for-byte (exit code
> ignored, exactly like the oracle). Identical → prints the captured output once + `parity OK (serial ==
> M:N)` to stderr, exit 0 (a held parity is a pass even if BOTH engines errored identically). Diverged →
> a greppable `parity DIVERGENCE (serial != M:N)` report (first differing stream + line, serial-vs-M:N
> side by side), non-zero exit. **Additive** — no VM/checker/concurrency change, seam is `src/main.rs`
> (new flag + `run_check_parity`/`report_stream_diff`) + `tests/check_parity.rs` + docs. Mutually
> exclusive with `--serial`/`--parallel`; `--threads=N` still sizes the M:N leg. **Limitation:** both
> legs share the one real stdin fd sequentially → a stdin-reading program diverges by construction (used
> as the negative test). A divergence is a signal to investigate (order-dependence / airlock / scheduler
> / accepted `--parallel`-only path), not automatically a bug. Tests: `tests/check_parity.rs` (conflict,
> OK on `concurrent_jobs.chz`, stdin-drain divergence). Docs: this note, `docs/concurrency.md` §6g,
> `chezzi run` `--help`.

> **✅ CONCURRENCY (2026-07-22) — `std.concurrency.task`: result handles for `Executor` work.** Bare
> `Executor.submit(f)` is fire-and-forget (returns nothing); `submit_task[T](ex, f) -> Task[T]` returns a
> future-style handle. **Pure Chezzi** (`std/concurrency/task.chz`) over the cap-1 bounded `Channel[T]`
> just landed — a one-shot result slot: `submit_task` builds over `Executor.submit_result` (which wraps
> the closure to `ch.send(f())`, added 2026-07-22), hands back a `Task{ch, cached}`. `Task.get() -> T` blocks then **memoizes** (idempotent — a 2nd call returns the
> cache, never a 2nd `recv` on the drained slot); `Task.done() -> bool` polls `ch.len() > 0` (non-block).
> **Parity:** a task's value is deterministic (`f()`), only its timing varies — so `.get()` is serial==M:N
> byte-identical *iff awaited in a fixed (submission) order*; deliberately **no `join_next()`**
> (completion-order = parity-hostile). Canonical shape: submit all → `shutdown()` → `.get()` each. No
> native/VM change (like `pmap`). Tests: `src/vm/tests.rs` `task_submit_get_submission_order_both_engines`,
> `task_get_idempotent_and_done_both_engines`. Docs: this note, `docs/concurrency.md` §5b, `docs/stdlib.md`.
> **Deferred:** making `Executor.submit` itself return `Task[T]` natively (needs a native/VM change +
> always-alloc a result channel on every detached submit — the free helper avoids both).

> **✅ CONCURRENCY / AIRLOCK (2026-07-21, `auto-task/module-global-generator-sendable`) — `docs/gaps.md`
> backlog item **B** CLOSED: a MODULE-GLOBAL live generator now crosses the airlock BY VALUE (deep copy),
> exactly like a frame-local one (F3 path C). The reach-gate + Option-B poison→`nil` model is RETIRED.**
> `to_snap_depth`'s fast path no longer excludes generator-embedding values (`!value_embeds_generator`
> clause dropped), so a handle-free module-global generator with all-sendable parked slots rides the
> `SnapValue::Wire(to_wire…)` lane. The slow `Obj::Generator` arm, however, snapshots a NON-sendable
> module-global generator (non-sendable parked slot / reference cycle / parked host handle) as an inert
> `SnapValue::Wire(WireValue::Nil)` placeholder — it must NOT re-raise the `to_wire` reject there, because
> `snapshot_modules` walks EVERY global once at the first `spawn`, so eager-faulting would abort any program
> that merely HOLDS such a generator without sending it (a regression a review round caught on the real
> binary — see the item-B remediation note below). So an untouched non-sendable global runs CLEAN; a task
> that REACHES it faults recoverably at the use site (`cannot iterate over nil`), byte-identical serial ==
> M:N ("fault only when reached"). Memory safety is by-value deep copy for the sendable case: `from_wire`
> rebuilds a fresh `GeneratorCore` on the worker heap (no shared cross-heap `GcRef`); a non-sendable one is
> inert `Nil` on every worker, so no cross-heap handle escapes. Each task already
> snapshots every module global per-task (`ensure_snapshot`, both engines since `6dca22c`), so two tasks
> reaching the same SENDABLE module-global generator each drive their OWN independent copy — `serial == M:N` by
> construction (verified byte-identical on the real binary, both engines). **The stale MED-HIGH risk
> premise** — a "serial=shared-ref vs M:N=by-value-copy divergence" (why `7b73e7c` kept the
> `value_embeds` clause + `Poison`) — was dead after `6dca22c` made serial snapshot per-task too.
> **Net-deletion:** removed the whole reach-gate — `check_task_generator_reach`,
> `check_outer_pending_generator_reach`, `check_task_reach_conservative`, `scan_proto_reaches_generator`,
> `proto_reaches_generator(+_rec)` + resolve/scan helpers, `any_hook_reaches_generator`,
> `any_module_global_embeds_generator`, `module_slot_embeds_generator`, `value_embeds_generator`,
> `gate_executor_queue` (netio), the `has_generators` VM field, and the `SnapValue::Poison` variant (the
> inert placeholder reuses `SnapValue::Wire(WireValue::Nil)`) — plus
> the ~30 now-obsolete reach-gate `*_faults_both`/`genreach_*` tests (their scenarios now cross or fault
> at the reach site). New parity RUN tests (serial == M:N, `src/vm/parity_tests.rs`):
> `generator_module_global_{reached_crosses,suspended_reached_resumes,two_tasks_independent_copies,
> parked_slot_nonsendable_rejects,in_data_cycle_rejects,unreached_nonsendable_runs_clean,via_executor_crosses}_both`
> + `generator_cross_module_member_call_crosses_both`. **Remediation (main-loop judge, post-review):** the
> auto-task's first cut made the slow `Obj::Generator` arm re-raise the `to_wire` reject, which regressed
> any program merely HOLDING a non-sendable module-global generator (confirmed on the real binary: a cyclic
> global generator + an unrelated `spawn` that never touched it faulted on the branch but ran clean on main).
> Fixed by the inert-`Nil` fallback above; a review prosecutor had flagged exactly this (defense wrongly
> dismissed it — re-verified by hand per `auto-task-review-unreliable`). The memories
> `generator-airlock-option-b-reach-gate` + `airlock-sendability-architecture` describe the RETIRED model.

> **✅ CONCURRENCY / SOUNDNESS (2026-07-21, `auto-task/serial-module-globals`) — `docs/gaps.md` §B3
> CLOSED by construction: the SERIAL engine now snapshots module globals per spawned task, matching M:N.**
> Root cause: a cooperative child aliased the shell's real `module_objs` while an M:N fiber installed its
> own snapshot — so a task mutating a module global leaked on `--serial` (shared) but was lost on M:N
> (snapshot), a `serial ≠ M:N` divergence. Fix (approach **b**, superseding the 2026-07-17 checker
> freeze): `join_nursery`'s serial branch reuses the SAME memoized `ensure_snapshot` M:N uses (module
> globals freeze once at the first nursery — NOT a fresh per-nursery `snapshot_modules()`, which would let
> serial read a between-nursery / pre-nested-spawn mutation that M:N's frozen memo hides) and a new
> `prepare_serial_child` deep-copies the module globals into each child's OWN `module_objs` view **in the
> shared heap** (reusing the exact M:N `to_snap` lowering + eager `fault_module`); `swap_ctx` now swaps
> `module_objs`/`module_faulted` **unconditionally** (per-fiber, cooperative or M:N), and `root_ctx` roots
> `ctx.module_objs` so a parked child's shared-heap copy (and the parent's real modules) survive GC.
> `serial == M:N` **by construction** for every mutation form — a task mutates its private copy; the
> parent is untouched on both engines. This holds for EVERY task-entry path: the cooperative
> `Executor.shutdown` inline drain (`src/vm/netio.rs`) also runs each submitted task under a fresh per-task
> child module view (`with_serial_child_modules`, the serial analogue of M:N's `drain_executor_on_pool`),
> so a module global mutated inside an `Executor.submit` closure isolates too
> (`executor_submit_module_global_inplace_mutation_isolates_parity`,
> `executor_submit_module_global_callee_reassign_isolates_parity`,
> `executor_submit_atomic_visible_to_parent_parity`). The escape hatch is unchanged: `Shared`/`Atomic`/`Channel` cross by
> shared `Arc` (via `to_snap`), so a task-side `a.add(1)` on a module-global `Atomic` IS visible to the
> parent. **GC safety:** the shell's real `module_objs`, swapped out during a serial child-modules
> window, are GC-rooted via a new `pinned_module_roots` field (`collect()` scans it) — the cooperative
> `Executor` **exit-drain runs with empty frames**, so frame-homes root nothing there; the pin keeps the
> invariant "`module_objs` is always valid" and closes a dangling-`GcRef` hazard the first (auto-task)
> attempt missed and was rejected for. (Honest scope: that hazard is latent — normal post-exit-drain flow
> never dereferences the stale refs, since downstream reads use the memoized heap-independent snapshot —
> so the pin is defense-in-depth, verified by adversarial review, not by a crashing test.) **Deleted** (were compensating for the divergence): `check_spawn_global_mutation` + its free-fn
> helpers, the method-mutation gate (`infer_method_call`), the index/field-assign gate (`check_assign`),
> the reassign gate, and their `rejects()` checker tests (net-negative lines). **Kept**: the local-capture
> sendability gate + `to_snap`'s Arc arms (the generator reach-gate + `SnapValue::Poison` are now GONE —
> see the item-B banner at the top of this file; module-global generators cross BY VALUE). New parity RUN tests (serial == M:N, `src/vm/parity_tests.rs`):
> `serial_module_global_method_call_mutation_isolates_parity` (residual A, cross-module fn call),
> `serial_module_global_spawned_callee_mutation_isolates_parity` (C), `..._task_local_alias_...` (D),
> `..._direct_mutation_forms_...` (list/map/struct/set/bytearray/reassign),
> `atomic_incremented_in_task_visible_to_parent_parity` (escape hatch), `nested_serial_spawn_...`, and
> `channel_park_keeps_module_snapshot_parity`; the freeze-timing (memoized, not fresh-per-nursery) is
> pinned by `nested_serial_spawn_mutation_before_nested_reads_frozen_parity` + `sequential_mutation_between_
> nurseries_reads_frozen_parity` (both serial≠M:N under a fresh snapshot — the bug the review caught).
> **Behavior change (honest):** a program that mutated a
> module global from a task and relied on the write propagating out used to work on `--serial` but was
> already broken (lost) on the shipping M:N engine — serial now matches M:N (no propagation). Residuals
> (A)/(C)/(D) resolved by construction. Docs: `gaps.md` §B3, `concurrency.md`, `syntax.md`, `spec.md`.
> **NEXT SESSION** (not this one) — sendability completeness, ranked, each its own spec: (1) protocol
> sendable under option (a) — Go `chan interface` parity, decision settled **[DONE]**; (2) recursive-local-fn
> sendability **[DONE 2026-07-21 — see below]**; (3) reject-case generators (mid-`recover:`/`defer`/multi-frame).
> Full backlog + decisions: `docs/gaps.md` "NEXT-SESSION BACKLOG".

> **✅ RECURSIVE-LOCAL-FN SENDABILITY (2026-07-21, `auto-task/recursive-fn-sendable`) — a nested recursive
> `fn` (and a mutually-recursive closure pair) now CROSSES the airlock and computes correctly on both
> engines.** Identity-preserving airlock serialization SCOPED to the `Obj::Cell` + `Obj::Closure` arms: a
> new `WireValue::Backref(u32)` + an `id` on the `Cell`/`Closure` wire arms. `to_wire_depth` threads a
> back-edge memo (`WireMemo` — an `FxHashMap<GcRef,u32>` DFS-stack set + id counter); on a revisit of a
> Cell/Closure still on the serialize stack it emits `Backref(id)` and stops. `from_wire` ties the knot:
> alloc a placeholder `Cell(Nil)`/`Closure(captured=[Nil;n])` FIRST, register `id→GcRef`, recurse children
> (a nested `Backref` resolves to the placeholder), then `heap.get_mut`-patch — **memory-safe** because
> `Heap::alloc` never collects (no GC between placeholder and patch) and `GcRef` is a GC-traced index, not
> a raw pointer (verified under GC stress). The old `graph_reaches_handle` reject + its two call sites +
> the fn are DELETED. **Design deviation from the literal spec (recorded):** the memo is BACK-EDGE-ONLY
> (pops a node off the stack on DFS exit), so only a TRUE cycle earns a `Backref`; an acyclic DAG alias
> (`[f, f]`) is deep-copied independently — preserving the Cell/closure deep-copy-independence contract
> (`airlock_aliased_closure_stays_independent` pins it) that a plain visited-set would have silently
> regressed. Corrected premise: there was NO pre-existing cycle-safe serializer to
> mirror; this is brand-new machinery. Tests: `airlock_recursive_local_fn_round_trips_both_engines` +
> `_under_gc_stress`, `airlock_mutually_recursive_pair_round_trips`, `airlock_recursive_closure_captures_
> outer_local_round_trips`, `generator_carrying_recursive_closure_round_trips_both`. **(Originally
> Struct/List/Map earned no id, so a pure-data cycle still tripped the depth cap; item A below GENERALIZED
> the machinery to every container arm — self-referential DATA now crosses too and `airlock_cycle.chz`
> FLIPPED to round-tripping.)** Docs: `gaps.md` §2 (→ DONE), `concurrency.md`.

> **✅ SELF-REFERENTIAL DATA SENDABLE (2026-07-21, `auto-task/self-ref-airlock`, gaps.md item A) — a self-
> referential struct/list/map/set/tuple/enum/newtype/cursor (`a.next = b; b.next = a`, a list holding
> itself, a map whose value refers to the map) now CROSSES the airlock and round-trips on both engines.**
> Generalized the recursive-fn `id`+`Backref` machinery from the `Cell`/`Closure` arms to **every container
> `WireValue` arm** (`List`/`Tuple`/`Map`/`Set`/`Struct`/`Enum`/`NewType`/`Iter`): each earns a per-
> serialization `id`; `to_wire_depth` inserts its GcRef into the `WireMemo` DFS stack BEFORE recursing
> (back-edge → `Backref(id)`, removed on DFS exit so an off-stack DAG alias stays an INDEPENDENT deep copy);
> `from_wire_memo` ties the knot in every arm (placeholder-alloc → register `id` → recurse → `heap.get_mut`-
> patch; `Map`/`Set` reuse the carried hash, never re-hashing a cyclic key). **NET-DELETION change:** the
> `WireMemo.nonpreserved_depth` field + BOTH mixed-cycle guards (commit e8dcad7) are GONE — a mixed
> struct+closure cycle now just round-trips. The `List`/`Tuple`/`Map`/`Set` tuple variants became struct
> variants `{id, items}`/`{id, entries}` so read-only match sites ignore `id` via `..`. **CORRECTED
> premise:** the spec's "`from_wire` already threads `rebuild` … tie-the-knot largely in place" was WRONG —
> the container arms recursed children BEFORE alloc, so the `from_wire` rewrite was the bulk of the work.
> `examples/airlock_cycle.chz` + golden FLIPPED (sections 1-3 round-trip). Depth cap STAYS as the backstop
> for genuinely-unbounded ACYCLIC nesting; the SOLE remaining value cycle that rejects is one threaded
> through a live **generator's parked frame** (no wire id) — caught by the `WireMemo.gens_on_stack` guard
> (re-entering the same generator on the serialize DFS stack → clean `a generator cannot be sent across
> tasks as part of a reference cycle` reject, NOT a silent duplicate: since the containers now
> back-reference, the container back-edge cuts the recursion before the depth cap would trip, so the
> generator arm guards the cycle directly — the fix for the adversarial-review reject of the first cut,
> which had deleted the mixed-cycle guard wholesale and let a gen+container cycle deep-copy the generator
> twice). Consequence flips: `airlock_cyclic_module_global_crosses_mn` (was `_recoverable_mn`),
> `airlock_cyclic_{struct,via_channel_send_and_shared}_crosses` (was `_recoverable`),
> `generator_parked_slot_nonsendable_rejects_both` re-pointed to a >10000-deep ACYCLIC parked slot. New
> tests: `airlock_self_ref_{struct,list,map}_round_trips_both`, `airlock_mixed_struct_closure_cycle_round_
> trips_both`, `airlock_struct_dag_alias_stays_independent` (adversarial parity-blind independence),
> `airlock_self_ref_struct_round_trips_under_gc_stress`, `generator_in_data_cycle_rejects_both` +
> `suspended_generator_in_data_cycle_rejects_both` (gen+container cycle reject). `src/vm/{wire.rs,sched.rs,
> fxhash.rs,core.rs,stmt.rs}`. Docs: `gaps.md` item A (→ DONE), `concurrency.md`.

> **✅ F3 PATH C (2026-07-20, `auto-task/generator-airlock-sendable`) — a LOCAL live generator is now
> SENDABLE across the airlock BY VALUE (deep copy).** The airlock VALUE serializer (`to_wire`/`from_wire`
> only) serializes a **frame-local** generator — `proto` (shared `Arc<Program>`), backing closure, and the
> parked operand-stack/args — and rebuilds an **independent `GeneratorCore`** on the receiving heap
> (advancing one copy never affects the other; proven both engines over `Channel[Iterator[int]]` + `spawn:`
> capture, Pending AND Suspended). Every parked slot is wired recursively, so a **non-sendable parked slot
> rejects AT SERIALIZE TIME** — the safer-in-direction property (a slot check can only over-reject, never
> under-gate). A suspension **inside a `recover:`** (a live handler stack) is ALSO sendable (backlog item 3
> arm b, 2026-07-21): a `Handler` is pure plain-data (`usize`-only, `Copy`, no `GcRef`/`Value`), serialized
> as-is and rebuilt coherently so the recover boundary resumes intact; `generator_next` rebases every parked
> handler/frame `nursery_len` to the resuming driver's floor (a generator opens no nursery, so its
> escape-drain must be a no-op — also fixes a latent same-heap over-drain that cancelled sibling `spawn`s
> when a mid-`recover:` generator resumed at a deeper nursery floor). The two remaining rejected shapes are
> **checker-unreachable** and kept only as defensive guards: a suspension **with a pending `defer`** (`defer`
> banned in a generator) and a **multi-frame** suspension (`yield` fires only in the generator's own frame).
> `to_snap`'s module-global path stays `SnapValue::Poison` for generators (F1 shared-ref contract
> intact) — but this needed a **main-loop judge-phase fix** (`7b73e7c`): making `to_wire` succeed for a
> sendable generator silently made `to_snap`'s wire **fast path** catch a module-global generator BY
> VALUE (bypassing the `Poison` arm, eroding the Option-B net); the fast path now excludes any
> generator-embedding value (`!value_embeds_generator`) so it stays Poison→Nil. The auto-task panel had
> DISMISSED this as unobservable — caught on independent re-verification of the merged HEAD (VM
> soundness: never trust the auto-task's own review). The reach-gate `check_task_generator_reach` is
> **retained** (now-redundant; its over-gate + doc cleanup is the remaining open F3 follow-up in
> `docs/gaps.md`). No checker change (`Iterator[T]` already
> sendable-permissive). No hot-path change (`CallFrame` already derived `Clone`; the diff is additive to
> the cold wire arms). Touched: `src/vm/wire.rs` (`WireValue::Generator` + `WireGenState` + `WireCallFrame`
> + `has_handle`), `src/vm/sched.rs` (`to_wire`/`from_wire`), `src/vm/core.rs` (`collect_core_gcrefs`),
> `src/vm/stmt.rs` (`display_wire`). ~15 module-global reach-gate faults + Poison→nil unchanged; the
> local-generator direct-crossing "graceful-fault" tests re-scoped to expect-success + deep-copy.

> **✅ BUG-HUNT (2026-07-20) — pre-JIT-freeze 5-domain adversarial hunt: 2 checker fixes + 1 doc fix
> landed, 1 held.** Five parallel subagents (airlock, cancel/defer, channel/nursery, checker⊋compiler,
> stdlib) swept the surface on both engines; airlock/channel/stdlib came back clean (consistent with 5+
> prior waves). Fixed:
> - **F1 — `?` in a `defer:` block was over-rejected by the *enclosing* fn's return type** (checker↔doc
>   drift). The `defer:` block is its own closure with a `?`-DISCARDING contract (`syntax.md`), but
>   `infer_try` validated the `?` against the enclosing `current_ret` — so it rejected under a nil/int
>   fn and only accepted under a `Result` fn *by coincidence* (wrong model — the runtime discards, never
>   propagates). Fix: an `in_defer_block` checker flag (mirrors `recover_depth`; reset at fn/closure
>   boundaries, zeroes `recover_depth` on entry since the block can't target an outer recover) makes
>   `infer_try` discard the `?` (accept any Result/Option, yield the payload, no enclosing-return
>   constraint). Checker-only, parity-neutral; runtime discard verified byte-identical on both engines.
> - **F4 — `int()`/`float()`/`bool()` accepted an AGGREGATE arg (List/Map/Set/tuple) at check, faulted
>   at runtime** (check-OK-then-run-fault). Those types are outside the scalar-cast domain and can never
>   convert (unlike a `struct`, whose `Convert` witnessing is a documented deferral), so the runtime
>   always faulted. New `reject_aggregate_scalar_cast` turns it into a clean compile error.
> - **F2 (doc) — `Shared.update` lock semantics + reentrancy limit** were only documented under
>   `RwShared`; added the note at `Shared.update` itself (`docs/stdlib.md`): `update(f)` runs under the
>   box's exclusive write lock, and re-touching the same box inside `f` self-deadlocks (M:N hangs;
>   `--serial` silently loses the inner write).
> - **F3 (HELD) — generator reach-gate over-gates** (any task that makes a call/captures faults if any
>   module-global generator exists), contradicting the docs' "an untouched generator global does not
>   fault." NOT fixed: tightening the reach analysis risks an unsafe *under*-gate (a live generator
>   crossing the airlock = VM frames on another thread), the exact hazard the gate over-approximates to
>   avoid — too risky pre-freeze. Needs a scoped precision spike; see `docs/gaps.md`.

> **✅ CHECKER / CONCURRENCY (2026-07-21, `auto-task/protocol-sendable`) — Task 2: ALL user protocol
> existentials are now sendable (Go `chan interface` parity), generalizing the earlier `Error`-only
> rule.** `Channel[P]`, protocol-typed spawn args / struct fields / `Ok`/`Err` payloads / returns all
> type-check — the erased witness crosses the airlock by deep value copy like any other value. **One
> logic line** (not a widening-site sweep, and NOT the risk the original backlog note framed): deleted
> `sendable_bounded` (`proto.rs`), flipped `sendable_rec`'s `Ty::Protocol` arm to `true`, and kept
> `assignable`'s existing `&& self.sendable(a)` concrete-witness guard uniformly. **Corrected framing:**
> `assignable` is the SOLE concrete→Protocol widening chokepoint (no coverage risk), and the CHECKER
> marks FFI/`Func`/handles **sendable** — the **runtime airlock** (`ensure_crossable` over `has_handle`)
> is the real gate for a genuinely-unserializable witness (FFI handle in a field, mid-`recover:`
> generator), rejecting it recoverably and identically on serial == M:N. Post-change `sendable_rec` is
> `false` only for `Ty::Module`. **Bench-neutral** (checker-only, no VM change). Migration: 14 old-policy
> tests flipped to accepted (each a genuinely-sendable witness — the old rejection was a false positive);
> genuine-rejection coverage moved to `vm::parity_tests::ffi_handle_cannot_cross_airlock_three_engine`.
> Full suite green. Details: `docs/gaps.md` item 1 + §L7 (superseded banner).

> **✅ CHECKER / CONCURRENCY (2026-07-20, `feat/l7-sendable-error`) — L7: sendability-bounded `Error`
> existential landed; `Channel[int!]` / `Channel[Error]` now admitted and sound.** *(Superseded
> 2026-07-21 by Task 2, which generalized this to ALL protocols — see entry above.)* The built-in `Error`
> protocol was **sendable-bounded** (`Error`-only, by default): its existential is itself sendable AND
> every value widened into it is required sendable *at the widening site*, so a struct that satisfies
> `Error` but holds a non-sendable field (a non-`Error` protocol / `Module` field) is **rejected there,
> not laundered** across a task boundary (closes the F2 check-OK-then-run-fault). Design ("Option B", 5
> checker edits): inference sites **preserve** a non-sendable concrete error (in-task use stays legal);
> the explicit/direct-literal widening chokepoint (`assignable`'s `Protocol` arm) rejects. Commits
> `c1b4ab4` · `997e642` · `2b29ed3` · `ba2ea7c`. Details + deferred follow-ups: `docs/gaps.md §L7`.

> **✅ LANGUAGE (2026-07-19, `auto-task/remove-ref`) — the `ref` keyword, the `Ref[T]` reserved box, and
> `std.ref` were REMOVED entirely (pure subtraction, minimalism/coherence — NOT a sendability change).**
> `ref T` (a binding modifier lowering to a `Ref[T]` box) and the explicit `Ref[T]` box only ever added
> **scalar** aliasing — a pointer-graft on Chezzi's Python object model, where structs/`List`/`Map`/`Set`
> already share by reference on assignment and scalars copy. Nothing real depended on it: **zero** stdlib
> `.chz` imported `std.ref`; the sole non-demonstrator example used `ref` only to show it now behaves like
> a plain local. Ripped across the whole pipeline: `Token::Ref` (lexer) + the `REF` grammar production
> (`docs/grammar.bnf`, conformance stays green); `Param.is_ref`/`StmtKind::Let.is_ref` (ast) + the parser
> eats; the entire `lower_refs`/`ref_names`/`callee_param_is_ref` desugar subsystem (variadic-collapse
> kept via a renamed `first_pass` flag); the checker `ref_decls`/`ref_seed`/`is_ref_decl`/`ref_display`/
> `check_ref_ty` plumbing + the `Ref` reserved-type arm + the dead `name=="Ref"` arm in `sendable_rec`;
> the resolver **always-link of `std.ref`** (+ its ordering tests) and `std/ref.chz` + the embedded-std
> entry; the compiler bare-`Ref` exposure block; and the VM airlock `captured_graph_embeds_ref` scanner
> (both `to_wire`/`to_snap` call sites, in lockstep). `Ref` is now an ORDINARY identifier — a user
> `struct Ref` is legal. For an in-task mutable value to close over / pass by reference use a plain
> one-field `struct` (a struct is a shared reference); for cross-task mutation use `Shared[T]`. The
> Channel/spawn sendability GATE is unchanged — its tests were re-expressed with a **protocol existential**
> probe (a genuinely non-sendable type) instead of `Ref[int]`. Boundary test
> `ref_surface_removed_fails_to_compile` pins that `ref`/`Ref[T]`/`import std.ref` now fail to compile
> with a clean error. Docs synced (syntax/spec/stdlib/concurrency/future/grammar); gaps.md L7 amended so
> the "wrong lever for F2" note isn't self-contradictory (the removal is orthogonal to L7/sendability).

> **✅ CHECKER (2026-07-18, `auto-task/checker-overreject-fixes`) — two disjoint over-rejection /
> diagnostic fixes (all checker-side, parity-neutral, runtime unchanged).**
> **F2 (dropped — was unsound) — ✅ SUPERSEDED by L7 (2026-07-20): the SOUND version landed.** See the
> L7 entry below / `docs/gaps.md §L7`. The naive whitelist was unsound because it erased field-level
> sendability; L7 ships the sound form (sendable-*bounded* `Error`: the existential is sendable AND
> every value widened into it is required sendable at the widening site). `Channel[int!]`/`Channel[Error]`
> now type-check and cross a task boundary; the `channel_of_protocol_existential_is_non_sendable` test
> was flipped to `channel_of_error_existential_is_sendable_but_other_protocols_not`. Original note kept
> for history:
> **F2 (dropped — was unsound)** — a proposed whitelist of the built-in `Error` existential as sendable
> (to admit `Channel[int!]`/`Channel[Error]`) was **rejected on review**: the `Error` existential erases
> field-level sendability, so a struct that satisfies `Error` yet carries a non-sendable field (a
> `Ref[T]`, a live generator) would launder past the gate that the concrete `Channel[MyErr]` correctly
> rejects — a check-OK-then-run-fault (`Err(GErr(gen()))` over `Channel[int!]` type-checked then faulted
> `a generator cannot be sent across tasks`). The conservative rejection is **correct and consistent**
> (an element type must be *provably* sendable; an existential is not — `Channel[Result[int,str]]` works
> because `str` is). Idiomatic error-over-channel = a concrete sendable error type (a typed enum).
> Pinned by `channel_of_protocol_existential_is_non_sendable`. **F3** — the free `unify` (`src/checker/mod.rs`)
> had no arms for the four native reserved generic handles, so `unify(Shared[T], Shared[int])` bound
> nothing and a generic fn/struct over `Shared`/`Channel`/`Atomic`/`RwShared` rejected the call (even
> with turbofish); the identical shape over `List[T]` worked. Fix: add the four handle arms to `unify`
> AND the matching subst arms to `subst` (the wrapper-struct field `ch: Channel[T]` now substitutes to
> `Channel[int]`). Audit: sibling walkers (`ty_collect_params`, `contains_unknown_in_slot`,
> `merge_unknown`, `sig.rs::fill_ret`) already list all four; `ty_fully_concrete` (mod.rs:2898) shares
> the `_ => true` shape omission but its bound-forwarding domain (where/satisfaction) is unreachable by
> handle types today (no `where`-bounded handle path) — left as-is (touching it risks bound-comparison
> behavior). **F4** (cosmetic) — `Atomic.add(1.5)` was correctly rejected but showed the List/Set
> collection element-pin hint (`add` collides between `Set.add` and `Atomic.add`). Fix: thread an
> `is_collection` bool through `check_args_range_decl` (new `check_args_range_coll` wrapper routes only
> the List/Set method arms); handle methods now never show the hint. Mismatch text/span/rc unchanged;
> `Set.add`'s hint still fires. Tests: `channel_of_protocol_existential_is_non_sendable`,
> `generic_fn_over_native_handles_infers_param`, `generic_wrapper_struct_holding_channel_substitutes`,
> `atomic_add_mismatch_no_collection_hint` (checker) +
> `generic_fn_over_native_handles_run_parity`, `generic_wrapper_struct_channel_run_parity` (RUN parity,
> serial==M:N). `cargo test`/`clippy`/`conformance` green.
>
> **✅ DRIFT-FIX (2026-07-19, `auto-task/gen-reach-argc-methods`) — generator-reach gate EXTENDED to
> argc>0 direct global calls + builtin container methods (still zero under-gate).** Follow-up to the
> gate below: `Vm::proto_reaches_generator_rec` (`src/vm/sched.rs`) still OPAQUE'd out on `Call(argc>0)`/
> `SpawnCall(argc>0)`, on every argc>0 `CallMethod` (builtin `push`/`pop`/`map`/…), and on list/arith
> ops — so a task doing `takes(5)` / `print(square(3))` / `xs.push(4)` / `xs.map(cleanfn)` wrongly
> faulted. Fix (all sound-conservative, over-gate OK / under-gate = bug): (1) `Call`/`SpawnCall` any
> argc — resolve the callee via a straight-line single-push operand window (`resolve_global_call_callee`;
> misalignment → OPAQUE) and recurse into the known module-global `Func`/`Closure`; (2) `CallMethod`
> any argc — INERT only if `name` is no struct-field name (`struct_field_names_contains`, the by-name
> over-approx that keeps fn-typed-field calls `recv.field(args)` OPAQUE **without resolving the
> receiver** — the exact serial≠M:N hole that sank a prior attempt), no generator-reaching user impl of
> `name`, and all callable args clean (`method_arg_reaches_generator` resolves a `GetGlobalSlot` fn arg
> and recurses — catches `xs.map(dirty)`); (3) list/tuple literals unconditionally inert; map/set
> literals + arith/`compare`/`in` (incl. fused `BinLocal*`/`IncLocal`) inert only when no
> operator-overload/`hash` hook can reach a generator — the `print_hazard` flag broadened to
> `hook_hazard` and its producer `any_str_hook_reaches_generator`→`any_hook_reaches_generator` (scans
> `str`/`add`/`sub`/`mul`/`div`/`mod`/`neg`/`compare`/`contains`/`hash`). **Adversarial-review round 2
> fixed 3 confirmed under-gates (serial-clean / M:N-fault soundness holes):** (i) a builtin `CallMethod`
> that re-enters a RECEIVER-element hook (`xs.sort()`→`compare`) was not gated — the arm now also gates
> on `hook_hazard` (invisible to the name/arg checks); (ii)/(iii) a **conditional-expression** callee
> `(if c: dirty else: clean)(…)` / higher-order builtin arg `xs.map(if c: dirty else: clean)` slipped
> past `resolve_global_call_callee` / `method_arg_reaches_generator`, which read a FIXED operand slot
> (the else-branch push) and missed the then-branch producer — both operand windows now require
> `window_has_no_incoming_jump` (no branch may land inside the window; an `if`-expr merge point → OPAQUE/
> gate). Tests: 20 new (`genreach_*` — A–F clean, G–N/P1–P3 fault, + 3 review-bug repros
> `genreach_builtin_sort_hook_reentry` / `_conditional_callee` / `_conditional_method_arg`, all
> RED-first then GREEN, faulting on both engines). Manual CLI: bug1/bug2 fault identically serial+M:N,
> B prints 9 both. Docs: `concurrency-b3.md`. Full `cargo test`/`clippy`/`conformance` green.
> **Adversarial-review round 3 fixed 1 more confirmed under-gate:** a cross-module member call
> `mod.fn(args)` — `CallMethod{name:"fn",argc}` preceded by `GetGlobalSlot(mod)` — slipped every
> `CallMethod` guard (module fns live in no method table; the receiver `GetGlobalSlot(Module)` is not
> a generator-embedding slot), so a spawned task calling a module-global fn that read a generator in
> ITS home module was serial-clean (prints 13) / M:N-fault (nil-iterate). Fix: modules are first-class,
> so the `CallMethod` arm now, when `name` is a member of ANY module (`module_member_name_exists`),
> resolves a DIRECT `GetGlobalSlot→Module` receiver (`resolve_module_member_callee`) and recurses into
> the member's own home — an INDIRECT receiver (`m := mod; m.fn()`, spawn arg) is unresolvable → OPAQUE
> gate (the P2-class hole for modules, NEVER receiver-resolved). Also closes the pre-existing argc==0
> `mod.baz()` sibling hole. Tests: 4 new cross-module parity tests (`genreach_cross_module_*` — direct
> + argc0 + alias fault, clean member call still runs). Full `cargo test` (3715 lib)/`clippy`/
> `conformance` green.
>
> **✅ DRIFT-FIX (2026-07-18, `auto-task/gen-reach-recurse`) — generator-reach airlock gate OVER-FIRED
> (check-OK-then-run-fault).** `Vm::proto_reaches_generator` (`src/vm/sched.rs`) opaqued out at the
> FIRST call/method/operator/nursery op (`_ => return true`), so a spawned task that merely called a
> generator-free user fn wrongly faulted `a generator cannot be sent across tasks` on BOTH engines —
> violating the doc contract (`concurrency.md`: "an untouched generator global does NOT fault").
> Fix: the scan now RESOLVES + FOLLOWS the transfers it can prove — a direct `Call(0)`/`SpawnCall(0)`
> of a known module-global fn (resolve the live slot to its `Func`/`Closure`), a `Type.method()`
> static, an `argc==0` method (by-name over all user impls, mirroring the `str`-hook scan), a static
> `spawn:` block — recursing memoized + cycle-guarded into the callee's OWN home; nursery-management
> ops are inert. UNRESOLVABLE/dynamic transfers stay OPAQUE (argc>0 calls + callable args, operator
> overloads, index/field/hash hooks, builtin re-entry, `spawn recv.m()`, `GetCaptured`), and every
> `Spawn*` op stays OPAQUE in the conservative outer-nursery TOCTOU mode — never under-gates. Tests:
> 4 new (`generator_reach_*` — clean-fn, builtin-method-on-local, by-name-method-reads-g-faults,
> nested-spawn-clean), all existing generator-reach positives stay faulting (now via resolved
> transitive reach). Docs: `concurrency-b3.md`, tests.rs comments. Full `cargo test`/`clippy`/
> `conformance` green.
>
> **✅ SOUNDNESS (2026-07-18, `auto-task/try-nil-fn-reject`) — `docs/gaps.md` §B6: `?` in a nil fn
> silently swallowed the error (check-OK-then-data-loss).** The checker accepted `?` whenever the
> enclosing return was `Nil` — but `Nil` covers BOTH module top-level (legit — the runtime unwinds the
> unhandled `Err`/`None` at the program boundary) AND a nil-returning fn body (the bug — the propagated
> `Err`/`None` was dropped). Fix: new checker signal `in_fn_body: bool` (false at module top-level, true
> inside any fn/closure body), saved/restored 1:1 beside every `current_ret` `mem::replace`
> (`check_fn_body`/`infer_fn_ret`/closure-infer) + reset in `begin_module`; the two `Ty::Nil => {}`
> acceptance arms in `infer_try` are now `Ty::Nil if !self.in_fn_body => {}`, falling through to the
> existing reject (`'?' used in a function that returns nil, not Result or Option`). No `fn main`
> exception — a fn must return `Result`/`Option` to use `?` (closures already enforced this). Runner
> symmetry: `Vm::invoke_entrypoint` now routes a manifest `module:function` entry fn's return through
> `top_level_error`, so a returned `Err`/`None` surfaces as `unhandled error: <msg>` (rc=1) — letting an
> entrypoint legitimately be `-> T!` and use `?` (both engines, one edit). Migrated `examples/hello.chz`
> + `examples/socket_timeout.chz` to `-> int!` + `return Ok(0)` (output-identical goldens hold).
> Docs: `docs/syntax.md` §9 `?` rule, `docs/spec.md` entry model + the safe_div example, `docs/gaps.md`
> §B6. Checker + runner change → parity preserved; `cargo test`/`clippy`/`conformance` green.
>
> **✅ LANGUAGE (2026-07-18, `auto-task/python-float-fmt`) — Python-compatible float formatting.**
> Two float-format defects fixed together behind ONE shared exponent-normalizer (`fmtspec::normalize_exp`):
> (1) the `{:e}`/`{:E}` spec was Rust-style (`1.23456789e5`) — now CPython-style: **default precision 6**,
> exponent **always signed + zero-padded to ≥2 digits** (`{123456.789:e}` → `1.234568e+05`, `{1.0:e}` →
> `1.000000e+00`, `{0.000123:.2e}` → `1.23e-04`); `E` is a NEW type char (uppercase marker, same exponent).
> (2) plain `str(float)`/`print`/`{f}`-no-spec/`json.stringify` never used scientific notation (Rust Display
> never does) — now **matches CPython `repr()`/`str()` exactly** (`fmtspec::repr_float`): scientific when the
> decimal exponent is `< -4` or `>= 16`, fixed otherwise, whole floats keep `.0`. `str(1e16)`→`1e+16`,
> `str(1e15)`→`1000000000000000.0`, `str(0.00001)`→`1e-05`, `str(1.5e300)`→`1.5e+300`, `str(-2.5e-8)`→`-2.5e-08`.
> `json.stringify` of a parsed `1.5e300` now emits `1.5e+300` (valid JSON, round-trips) not a 300-digit decimal.
> Single-sourced: `vm::format_float` delegates to `fmtspec::repr_float`; both the spec arm and repr path share
> `normalize_exp`. This is a **deliberate reversal** of the old "never scientific" divergence (commit 4f1ec35) —
> Python parity is the goal. CPython differential shim (`emit_python.rs`) switched to `repr(v)` in lockstep.
> Goldens updated: `examples/{float_large_integral,literals,format_specs}.expected`. Docs: `docs/syntax.md`,
> `docs/spec.md`. Two-engine parity green (serial==M:N, one shared formatter). `cargo test`/`clippy`/`conformance` green.
>
> **✅ LANGUAGE (2026-07-17, `auto-task/check-fmtspec`) — Nit 2: static format-spec/value-type check.**
> A `{expr:spec}` interpolation whose spec is provably wrong for a **concrete scalar** value
> (`{s:.2f}` on a str, `{x:d}` on a float, `{x:.3d}` precision on an int) is now a **compile error**
> (`chezzi check`), not runtime-only — consistent with the already-static width>4096 cap and Chezzi's
> statically-typed model (deliberate divergence from Python's runtime `ValueError`). Single-sourced:
> new `fmtspec::spec_valid_for_scalar(spec, ScalarKind)` predicate; `render_int/float/str` call it
> first (runtime wording unchanged, guard arms now `unreachable!()`); the checker (`check_interpolation`)
> calls the SAME predicate, but ONLY for `Ty::Int/Float/Str/Bool` — `Unknown`, a generic `Param(T)`,
> protocols, structs, lists, bytes fall through and keep the runtime backstop (a generic `"{v:.2f}"`
> body is NOT statically rejected; instantiating it with str still faults at runtime, both engines).
> Docs: `docs/syntax.md` (format specs). Checker-only + pure runtime refactor → parity untouched.
> `cargo test`/`clippy`/`conformance` green.
>
> **✅ PARITY (2026-07-17, `auto-task/b4-executor-backtrace`) — `docs/gaps.md` §B4: converge the
> `Executor`-task uncaught-error backtrace frames across engines (was: cosmetic serial≠M:N).** An
> uncaught fault from an `Executor.submit(...)` closure printed a full backtrace on `--serial`
> (`at boom`/`at <closure>`/`at main`) but only `at main` on M:N (same message/location/rc). Serial's
> `shutdown` drains each submitted task INLINE on the entry `Vm`, so the task's callee frames were
> captured into `fault_trace` while intact; M:N runs each task on an isolated worker `Vm` and drops that
> trace. Fix (VM-only, `src/vm/netio.rs` serial shutdown drain loop): snapshot any pre-existing
> `fault_trace`/`fault_trace_depth`, give the inline task a clean slate, then RESTORE the snapshot —
> dropping only the inline task's own frames, never a superseding outer fault. Three cases converge:
> explicit `ex.shutdown()` → both `at main`; `defer ex.shutdown()` while `main` unwinds → both `at main`
> (snapshot/restore preserves the outer `[main]`; an initial `= None` clear got this wrong, serial `[]`
> vs M:N `[main]` — caught in review); implicit end-of-program `drain_live_executors` → both EMPTY (no
> enclosing `run_until` to re-capture, parity holds at `[]`). Message/location/rc unchanged.
> Test: `executor_task_fault_trace_matches_on_both_engines` (all 3 cases + nursery neighbor guard) in
> `src/vm/parity_tests.rs`. Full `cargo test`/`clippy`/`conformance` green.
>
> **✅ SOUNDNESS (2026-07-17, `auto-task/b3-frozen-aggregate-mutation`) — `docs/gaps.md` §B3: freeze
> IN-PLACE mutation of a captured module-global aggregate in a task (was: serial≠M:N divergence).** The
> frozen-module-global rule (checker already REJECTED *reassigning* a captured module global inside a
> `spawn`/`parallel:` task) now ALSO rejects **in-place mutation** — `.push`/`.add`/`m[k]=v`/`s.field=x`
> (nested) whose receiver root is that global. Closes the half-enforcement that let the write leak on
> `--serial` (shared by ref) but silently vanish on M:N (per-task snapshot). Checker-only, **parity-neutral**
> (no VM / dispatch / deep-copy change): three gates reuse the existing `is_captured && !is_local_capture`
> module-global-only boundary — method-mutation in `infer_method_call` (typed on receiver ∈
> {List,Map,Set,bytearray} + a mutator name, so `Shared.update`/`Atomic.add`/user methods can't false-fire),
> index/field-assign at the top of `check_assign`, and index/field-assign in the transitive-callee scan
> `check_spawn_global_mutation`. A **fn-LOCAL** aggregate stays accepted (deep-copies per task, agrees on
> both engines). Covers the direct repro, `Executor.submit` closures, closures declared inside a `spawn:`
> block, and `spawn f()` callee index/field-assign. **Residual v1 gaps** (same pre-existing indirect-dispatch
> class, documented in `gaps.md §B3`): top-level-bound closure `spawn w()`, closure via captured struct
> field, callee-form *method*-mutation, and a task-local alias `local := xs; local.push()`. Tests: 6 checker
> rejection + 5 boundary/pin-accept unit tests +
> `module_global_aggregate_mutation_in_task_parity` (serial==M:N==3, the accepted fn-local path). Docs:
> `gaps.md §B3` (→ FIXED core forms), `concurrency.md §7`. Full `cargo test`/`clippy`/`conformance` green.
>
> **✅ LANGUAGE (2026-07-17, `feat/const-binding`) — `docs/gaps.md` L4: `const` immutable bindings.**
> `const T` is a binding modifier in the same type-slot as `ref` (`PI: const float = 3.14`); the
> checker rejects any later reassignment of the name (`=` + every compound). Immutable *binding*, not a
> compile-time constant — runtime RHS is fine (JS `const`/Java `final`), and **shallow** (freezes the
> name; a `const` container's contents stay mutable). Locals + module globals only; parse-rejected on
> params, `:=`/destructuring, and `ref const`. Const-ness rides `ModuleSig.const_values`, so a
> from-import / qualified rebind of a `const` global (or a native constant `math.pi`/`e`/`inf`/`nan`)
> reports it as const, not as a mutable snapshot/field. Mirrors the `ref` `const_decls` sidecar
> end-to-end; **compile-time-only, zero VM change, byte-identical on both engines**
> (`golden_const_binding_via_run_file`). Visibility (`pub`/`_`) deliberately deferred until R3. Docs:
> `syntax.md §const T`, `grammar.bnf`, `stdlib.md §std.math`, `examples/const_binding.chz`.

> **✅ ENGINE + STDLIB (2026-07-17) — HYBRID native+Chezzi std module (a `std/*.chz` may mix bodyless
> `native fn` decls with BODIED Chezzi `fn`s); first user: `math.divmod`.** Resolves the architectural
> fork where a native module was harvested for SIGNATURES only then `continue`d past `check_module`
> (`src/checker/mod.rs`) — so a top-level bodied `fn` was dropped and native-struct `bodied_methods`
> (`io.Reader.lines`) had their bodies UNCHECKED (a `str`-under-`int` return slipped through — the
> soundness hole a prior spike found). Three seams: (1) harvest PASS 2b reads module-level `StmtKind::Fn`
> into `sig.functions` (`setup.rs`); (2) the native arm now runs `check_fn_body` on a clean `begin_module`
> env for BOTH module-level bodied fns AND native-struct bodied methods — the method case uses the
> RESERVED self-Ty (`qualified_builtin_ty`, `Reader`→`Ty::Reader`) so `self.read_line()` dispatches via
> the reserved-handle arm, and a FRESH self-carrying `fn_sig` (not the leading-`self`-stripped table sig,
> which would shift every param to `Unknown`); (3) `run_module` (`src/vm/exec.rs`) falls through to
> `run_proto` after injecting native members so the bodied fn's global binds (empty no-op for pure-native
> modules). `math.divmod(a,b) -> (int,int)` = `(a / b, a % b)` — no `NativeRet::Tuple` needed, closing
> `gaps.md §5`'s divmod deferral. IN scope both soundness sites. **Native files can now `import` too**
> (a native `.chz` is still a real file): `visit_native_file` resolves its imports via a new shared
> `resolve_ast_imports` helper (extracted from `visit`), the native-arm body-check binds them, and the
> compiler already carried `lm.imports` — a bodied fn there can `import std.string` and use it. **Adversarial
> review (post-commit) found + fixed two**: (1) a native↔native import cycle panicked in the VM
> (`visit_native_file` lacked `visit`'s cycle/depth guard) — extracted a shared `enter_module_guard`
> used by both, so a cycle now reports a clean `import cycle: …` error (regression test added); (2) the
> `divmod` docs falsely claimed Python semantics — Chezzi `/`/`%` are deliberately **C-style** (truncating,
> dividend's sign; `syntax.md:1347`), so `divmod(-7,2)` is `(-3,-1)` here vs Python's `(-4,1)`. Impl is
> correct (matches Chezzi's own operators — a floor variant would drift from them); docs corrected
> (`math.chz`/`stdlib.md`/`gaps.md`). Tests: in-process `entry_ok`
> (member/from-import resolution) + `tests/hybrid_native_module.rs` (6: divmod run-parity, `Reader.lines`
> run-parity, native-file import, native↔native cycle → clean error, and TWO `$CHEZZI_STD`-corrupted-copy
> RED soundness tests in an isolated CHILD PROCESS — env is process-global, must NOT be set in-library).
> `math_..._representative_sigs_exact` 31→32. Full `--lib` 3608 green, `hybrid_native_module` 6 green,
> clippy clean. Docs: `syntax.md` (native-struct-mix note extended + module-level hybrid), `stdlib.md
> §std.math`, `gaps.md §5`.

> **✅ STDLIB (2026-07-16, `auto-task/crypto-secure-random`) — `docs/gaps.md` §7: CSPRNG in
> `std.crypto`.** Two members added to the file-backed native `std.crypto` (bodyless `native fn` sigs
> in `std/crypto.chz`, impls in `src/native/crypto.rs`, zero new deps — libc `getrandom` like
> `uuid.rs::auto_seed`): `secure_bytes(n: int) -> bytes` (Python `secrets.token_bytes`) and
> `token_hex(n: int) -> str` (Python `secrets.token_hex`, 2n lowercase-hex chars, reuses the existing
> `to_hex` helper). Both share `secure_random_bytes(n)` which **FAILS CLOSED** — unlike `uuid.rs` it has
> NO weak `SystemTime` fallback: on `getrandom` `<0` (non-`EINTR`) / `0` it raises a recoverable
> `HostError` (catchable by `recover:`), never degraded bytes; the fill-loop retries the remainder on
> short reads + `EINTR`; `n<0` and `n>1<<20` (1 MiB cap) fault before allocating (no OOM); non-Linux
> arm faults (never weaken). Not in `is_blocking` (fast syscall, inline like sha/uuid). Output is
> INTENTIONALLY non-deterministic → NO byte-exact golden and NO `src/vm/parity_tests.rs` entry (serial
> vs M:N draw different bytes); tests assert PROPERTIES only — a Rust unit test (`secure_random_props`:
> length/uniqueness/empty/hex-alphabet/fail-closed) + `examples/crypto_secure_test.chz` (`chezzi test`,
> in the `d1_dogfood` list), both engines run clean (verified via CLI: `run` M:N + `--serial`, incl. the
> `recover:` fail-closed path). `token_urlsafe` (base64url) deferred. `crypto_fn_sigs_exact` 8→10, crypto
> `MEMBERS` list test updated. Full `--lib` green, clippy clean, conformance green.

> **✅ STDLIB HYGIENE (2026-07-16, `feat/timer-decl`) — `timer` now DECLARED in `std/time.chz`.**
> BEHAVIOR-PRESERVING consistency fix. `std/time.chz` declared its 4 real fns
> (now/monotonic/sleep_ms/format) but NOT `timer`, with a big NOTE calling it the exception —
> inconsistent with `Shared`/`Executor` which ARE declared (as `native struct`) despite being
> opcode-backed. Now `native fn timer(ms: int) -> Channel[bool]` sits next to its siblings. It stays a
> BARE-callable (`timer(50)`, not `time.timer(50)`), import-gated, `Op::NewTimer`-lowered, reserved,
> non-renamable builtin — so harvest (`harvest_native_module` PASS 2) routes its sig to a new
> `Checker.time_timer_sig` field, NOT `sig.functions` (which the From-import arm would bind as a normal
> callable, breaking bare-callability). The bare `timer(...)` expr arm now single-sources its arg/return
> types from that field (fallback = the old `[int] -> Channel[bool]` for the no-graph path); the
> import-license stays in the `native_module_sig("std.time")` `sig.types` insert. **Supersedes** the
> phase-4e note below ("timer DELIBERATELY NOT declared … would fault") — the fault only happens if it
> lands in `sig.functions`, which the harvest branch now prevents. Honest caveat: this RELOCATES the
> one-line special-case (into the harvest name-match), it doesn't remove it — `timer` is a category-of-
> one. Zero observable behavior change: only the resolver native-marker assertion
> (`enc_crypto_uuid_time_are_file_backed_with_native_marker`) flipped (must-declare); every sig-shape
> (`time_fn_sigs_exact`: functions still exactly 4) + both-engine runtime + golden timer test stays green
> unchanged. Full suite 3599 green, clippy clean, conformance green.

> **✅ STDLIB (2026-07-16, `auto-task/crypto-hash-hmac`) — `docs/gaps.md` §7: sha1 / sha512 / HMAC in
> `std.crypto`.** Five members added to the file-backed native `std.crypto` (bodyless `native fn` sigs
> in `std/crypto.chz`, hand-rolled impls in `src/native/crypto.rs`, zero new deps — same seam as the
> existing `sha256`/`md5`): `sha1(s)`/`sha1_bytes(b)` (FIPS 180-4, 5×u32 80-round; NOT
> collision-resistant — git/legacy only), `sha512(s)`/`sha512_bytes(b)` (FIPS 180-4, 8×u64, 128-byte
> block + 128-bit BE length pad — NOT sha256's 64/56/8), and `hmac_sha256(key: bytes, msg: bytes)`
> (RFC 2104, built over the existing `sha256_digest` primitive, block size 64, key-hash-first when
> >64). All hash → lowercase-hex `str`; `_bytes` twins via the `arg_bytes` seam; str inputs hash their
> UTF-8. Pure CPU ⇒ serial==M:N==interp trivially at the NativeFn seam. Tested against PUBLISHED
> vectors, not round-trips: `sha1_fips180_vectors`/`sha512_fips180_vectors` (incl multi-block pad
> guards) + `hmac_sha256_rfc4231_vectors` (RFC 4231 TC1/TC2 + TC6 131-byte key for the >64 hash-first
> branch) unit tests; `examples/crypto.chz`/`.expected` extended (`golden_encoding_crypto_via_run_file`
> + `assert_file_parity` = serial==M:N). Count-asserts bumped: `native::mod` name-list +
> `crypto_fn_sigs_exact`. **NOT shipped** (ponytail follow-up note in the module docstring):
> `hmac_sha1`/`hmac_sha512` (want a block-size param + `&[u8]` adapters — add if a caller needs them).
> **STILL OPEN §7**: secure-random-bytes / token, password hashing (bcrypt/argon2). Docs:
> `docs/stdlib.md §std.crypto`, `docs/gaps.md §7` (SHIPPED). Full `cargo test`/`clippy`/`conformance` green.
>
> **✅ STDLIB (2026-07-16, `auto-task/fs-stat-walk`) — `docs/gaps.md` §6: fs metadata READ + recursive
> walk.** Two natives on the file-backed `std.fs` + a new native struct `FileInfo`. `fs.stat(path) ->
> Result[FileInfo]` reads real filesystem metadata into `struct FileInfo { size, mtime, mode: int,
> is_dir, is_file, is_symlink: bool }` — FOLLOWS symlinks for size/mtime/mode/is_dir/is_file (matches
> `stat`/`os.stat`), `is_symlink` reported from a separate `symlink_metadata`; `mtime` = Unix-epoch
> secs (`0` if pre-epoch/unsupported), `mode` = raw unix `st_mode` (`0` non-unix); `Err` (recoverable)
> on a missing/unreadable path. `fs.walk(path) -> Result[List[str]]` recursively lists every entry
> under `path` as full path strings in a **deterministic per-dir-sorted, dir-before-children** order
> (required for serial==M:N parity — `read_dir` order is arbitrary); a symlinked dir is listed but NOT
> descended (cycle guard). `FileInfo` is import-gated (module-owned, not program-global) via the same 3
> layout copies + `assign_type_keys`/`type_names` bind_import-skip path as `Match`/`Response`/
> `ProcResult` (no is_reserved_type entry, no bespoke exec.rs skip). Tests: `native/fs.rs` unit
> (`fs_stat_reads_metadata`, `fs_stat_follows_symlink_but_flags_it`, `fs_walk_recursive_sorted`,
> `fs_walk_does_not_follow_symlink_dirs`) + `fs_stat_walk_fileinfo_parity` (RUNS `import FileInfo from
> std.fs` + stat/walk on BOTH engines — the reserved-type-hole regression guard). Docs: `docs/stdlib.md
> § std.fs`, `docs/gaps.md §6` (SHIPPED). Field order is load-bearing across `std/fs.chz` /
> `compiler/mod.rs` / `checker/setup.rs` / `native/fs.rs` — guarded by the parity test's size+is_file
> asserts.
>
> **✅ STDLIB (2026-07-16, `auto-task/std-duration`) — `docs/gaps.md` §9: Go-like first-class `Duration`.**
> NEW pure-Chezzi module `std/duration.chz` (zero native seam; only Rust touch = one `include_str!` line
> in `src/resolver/std_embed.rs`, guarded by `embedded_std_table_matches_disk`). `Duration` is a plain
> user struct over an int of **milliseconds** (matches `sleep_ms`/`timer(ms)`; i64-ms overflows at ~292M
> yr vs Go nanos' ~292 yr — the trade is a documented sub-ms ceiling: µs/ns `parse` → clean `Err`).
> Surface: constructors `millis/seconds/minutes/hours(n)`, method accessors
> `as_millis()/as_seconds()/as_minutes()/as_hours()`, arithmetic `add`/`sub`/`scale`, a Go
> `time.Duration.String()` formatter `to_string()` (`"1h30m0s"`/`"1.5s"`/`"250ms"`/`"0s"`/`"-1.5s"`) and
> its inverse `parse` (Go's looser forms — optional `+`/`-`, unordered summed `h`/`m`/`s`/`ms` groups,
> decimal/leading-dot magnitudes, bare `"0"`; clean `Err` on empty/no-unit/unknown-unit/multi-dot/
> trailing-dot, and a ≤12-digit int-part bound so an oversized magnitude is an `Err` not an i64 fault),
> plus `since(start: float)`/`sleep(d)` over native `std.time`. `parse`/`to_string` round-trip is the
> load-bearing surface (exact because source is integer ms). Tests: `examples/duration_test.chz`
> (5 `test fn`s, wired into `d1_dogfood_example_tests_pass`) + `golden_duration_via_run_file`
> (`examples/duration.chz`/`.expected` + `assert_file_parity` = serial==M:N). `parse` pre-collects
> codepoints into a `List` for O(1) indexing (avoids the pure-Chezzi `s[i:i+1]` O(n²) trap). Docs:
> `docs/stdlib.md § std.duration`, `docs/gaps.md §9` (SHIPPED). `sleep_ms`/`timer` stay int-ms (additive).
>
> **✅ STDLIB (2026-07-16, `auto-task/std-csv`) — `docs/gaps.md` §7: CSV read/write.** NEW pure-Chezzi
> module `std/csv.chz` (zero native seam — RFC 4180 quote state machine over the core `str` primitives
> + `std.string.replace`/`index_of`; only Rust touch = one `include_str!` line in
> `src/resolver/std_embed.rs`, guarded by `embedded_std_table_matches_disk`). `parse(text) ->
> List[List[str]]` (comma sep, CRLF **or** LF records, `""`→literal quote in quoted fields, spaces
> significant, trailing sep → no spurious record, empty input → `[]`, blank interior line → `[""]`) +
> `format(rows) -> str` (quote iff `,`/`"`/CR/LF, double embedded quotes, each record CRLF-**terminated**).
> Round-trip `parse(format(rows)) == rows` is TOTAL — proven in-Chezzi over every hard case (embedded
> comma/quote/newline, empty field, unicode) INCLUDING a sole/trailing empty record `[""]` (CRLF
> termination + parse's "trailing sep = no spurious record" rule; the earlier separator-join couldn't
> and was fixed pre-merge). `parse` pre-collects codepoints into a `List` for O(1) indexing (the
> per-char `s[i:i+1]` slice was O(n²) — a large CSV hung; also fixed pre-merge). Tests:
> `golden_csv_via_run_file` (`examples/csv.chz`/`.expected` +
> `assert_file_parity` = serial==M:N). Docs: `docs/stdlib.md § std.csv`, `docs/gaps.md §7` (SHIPPED).
> Deferred v1: streaming/Reader, header→Map mapping, custom-delimiter/TSV `parse_sep`.
>
> **✅ STDLIB (2026-07-16, `auto-task/std-os-sysfns`) — `docs/gaps.md` §6: OS / system fns.** Eight
> module-level natives on the file-backed `std.os`: queries `getpid() -> int`, `platform() -> str`
> (`std::env::consts::OS`), `hostname() -> str` (libc `gethostname`, `""` on failure — no new dep),
> `home_dir() -> Option[str]` (`$HOME` via the injected env), `temp_dir() -> str`, `environ() -> Map[str,str]`;
> mutations `setenv(key,value) -> nil` and `chdir(path) -> Result[nil]`. **Env-source consistency (the
> §6 point):** `env` / `environ` / `setenv` all read/write the SAME injected `HostConfig` env map — a
> `setenv` is observed by both `env` AND `environ` (two new DEFAULTED `Host` methods `os_environ`/`os_setenv`,
> overridden only in `VmHost`; no `std::env::set_var` third source). The env map is **shared** across M:N
> workers (`Arc<Mutex<…>>`, not a per-worker clone), so a `setenv` from inside a task is visible to the
> parent + siblings — process-global, matching the serial engine and Python/Go (serial==M:N, no parity
> break); `environ` sorts by key so both engines emit byte-identical output. `chdir` mutates the REAL
> process cwd (like `getcwd` reads it) — **process-global**, shared by all M:N workers (ponytail ceiling,
> same as Python/Go). Queries are engine-agnostic (serial==M:N). Tests: `golden_os_setenv_environ_consistency`
> (proves setenv↔env↔environ), `golden_os_setenv_visible_across_tasks` (setenv-in-task visible to the parent,
> both engines), `golden_os_environ_deterministic_order` (sorted, serial==M:N), `golden_os_queries` (shape +
> engine agreement), `golden_os_chdir` (Ok/Err under `FS_SCRATCH_LOCK` + cwd-restore). Example
> `examples/os_info.chz` (no `.expected`). Docs: `docs/stdlib.md §std.os`, `docs/gaps.md §6` (SHIPPED; the
> os.env/process.cmd note reworded — os.env axis resolved, child-process axis unchanged by design). Deferred:
> `os_name` alias, Windows `USERPROFILE`, signals/atexit, metadata-reader.
>
> **✅ STDLIB (2026-07-16, `auto-task/io-isatty`) — `docs/gaps.md` §6: TTY detection.** Three
> module-level bool natives on the file-backed `std.io` module: `io.isatty()` / `io.isatty_stdin()` /
> `io.isatty_stderr()` `-> bool`, each one line via `std::io::IsTerminal` on stdout/stdin/stderr. Lets
> a CLI colorize only when not piped (Python `sys.stdout.isatty()` / Go `isatty`). Engine-agnostic (an
> env fd query on the REAL OS fd, not a VM-sink query → serial==M:N trivially; value reflects the
> launching fds — a pipe/redirect → false, an attached terminal → true; libtest capture doesn't touch the fd).
> Seam reused as-is: `NativeRet::Bool` → `Value::Bool`, no new plumbing. Test `golden_isatty_via_run_file`
> asserts bool-shape + engine agreement (not a fixed value). Manual eyeball: terminal → all true; `| cat`
> pipes fd1 only → stdout false, stdin/stderr true. Docs: `docs/stdlib.md §std.io`, `docs/gaps.md §6` (SHIPPED). Deferred
> (deliberate second step): terminal-size / echo-off / raw-mode.
>
> **✅ STDLIB (2026-07-16, `auto-task/lazy-iter`) — `docs/gaps.md` §3: LAZY ITERATORS (itertools) in
> `std.iter`.** `std.iter` was all-eager (returns `List`); added lazy `Iterator[T]` adapters as
> pure-Chezzi **generators** (`yield`) in the existing `std/iter.chz` (it is pure-Chezzi — no native
> seam, no dead-code hazard): `count(start=0, step=1) -> Iterator[int]` (infinite counter),
> `repeat(x, n=-1)` (`x` forever if `n<0`, else `n` times), `cycle(xs)` (endlessly repeat a list;
> **empty list = immediately-done, not an infinite spin**), `chain(a, b)` (a then b; two-arg only in
> v1), `islice(it, stop)` (lazy prefix; `stop<=0` = empty — the terminator, via `break` inside the
> generator's `for`), and the lazy `imap`/`ifilter` (named to dodge the eager `map`/`filter` — Chezzi
> has no overloading). The `it`-taking adapters use the `[S: Iterable[T], T]` bound so they
> accept any iterable (list/set/str/user-`next()`/generator). (Originally `[S: Iterator[T], T]`; migrated
> by the 2026-07-26 W6-3b fix, which narrowed `Iterator` to real cursors — same accept set.) Laziness proven: `count()` (infinite) →
> `islice(_, 5)` terminates; `imap`/`ifilter` compose over `count()` and terminate under `islice`.
> Pure-Chezzi ⇒ serial-VM == M:N automatically; 5 inline `parity_entry` tests (both engines) in a
> labeled block. Dropped: `take(it, n)` alias (collides with eager `take`; `islice` covers it). Docs:
> `docs/stdlib.md § std.iter`, `docs/gaps.md §3` (SHIPPED). Full `cargo test`/`clippy`/`conformance` green.
>
> **✅ STDLIB (2026-07-16, `auto-task/std-bisect-memoize`) — `docs/gaps.md` §10: `std.bisect` +
> `std.memoize`, two pure-Chezzi modules.** Both are NEW pure-Chezzi files (zero native seam — the only
> Rust touch = two `include_str!` lines in `src/resolver/std_embed.rs`, guarded by
> `embedded_std_table_matches_disk`). `std/bisect.chz`: `bisect_left`/`bisect_right`/`bisect` (alias) +
> `insort_left`/`insort_right` over `List[T: Comparable]` (Python `bisect` semantics — left lands
> before equal elements, right after; `insort_*` is O(n) grow-then-shift since `List` has no native
> insert). `std/memoize.chz`: `memoize1(f: fn(K) -> V) -> fn(K) -> V` caches per distinct arg in a
> **captured `Map`** (native ref type ⇒ cache persists across wrapped calls; B3.3 closures-as-data) —
> `f` runs once per distinct arg. Pure-Chezzi ⇒ serial-VM == M:N automatically. Test vehicle = golden
> example + `assert_file_parity` (both engines) per module: `golden_bisect_via_run_file` (full boundary
> matrix: empty / all-equal / both ends / dup left-vs-right / in-place insort) and
> `golden_memoize_via_run_file` (single-eval proven by a captured call-counter → counter == 2 for two
> distinct args). v1 ceilings (in `ponytail:` comments): bisect has no key-fn / no bare `insort` alias;
> memoize is single-arg only (N-arg needs `Map[tuple, V]` but tuples aren't Hashable map keys yet).
> Docs: `docs/stdlib.md § std.bisect` + `§ std.memoize`, `docs/gaps.md §10` (both SHIPPED). Full
> `cargo test`/`clippy`/`conformance` green.
>
> **✅ STDLIB (2026-07-16, `auto-task/list-ergonomics`) — `docs/gaps.md` §2 wave-1: LIST value/ergonomics
> methods.** Nine methods added to the file-backed `native struct List[T]` seam (bodyless sigs in
> `std/prelude.chz` → name-keyed VM dispatch in `src/vm/call.rs`; zero new checker code — the generic
> `Ty::List` arm harvests sigs, `where T: Comparable` enforced via `enforce_bounds`, `min_by`/`max_by`'s
> `[K: Comparable]` routes through `infer_generic_method` exactly like `sort_by_key`): `min`/`max`
> (`where T: Comparable`; first-seen tie; empty faults `min()/max() of empty list`; `NaN` uses `sort()`'s
> total order, never faults), `min_by`/`max_by` (`fn(T)->K` key, returns the extremal **element**),
> `first`/`last` (`-> Option[T]`, `None` on empty), `reversed` (**new** list, receiver untouched — distinct
> from in-place `reverse`), `insert(i,x)` (Python-clamps, never faults), `remove_at(i)` (returns the element,
> Python-relative negatives, true-OOB faults with the shared `index {i} out of bounds (len {n})` message).
> GC discipline: `min`/`max` (struct compare re-enters the VM) + `min_by`/`max_by` (key extractor re-enters)
> root the source/snapshot/keys on the operand stack and re-fetch per iteration, mirroring
> `list_sort_structs`/`list_sort_by_key`. Tests: 8 inline dual-engine parity/fault tests (`assert_mc_parity`
> + `assert_fault_parity`) + extended `examples/list_methods.chz` golden (serial==M:N). Docs:
> `docs/stdlib.md § List[T]`, `docs/gaps.md §2` (SHIPPED bullets struck). **STILL OPEN §2**: `iter.min`/`max`,
> `unique`/`dedup`/`chunk`/`windows`/`group_by`/`partition`/`flat_map`/`take_while`/`drop_while`, Map/Set
> ergonomics — separate waves.
>
> **✅ STDLIB (2026-07-16, `auto-task/list-ergonomics`) — `docs/gaps.md` §2 wave-2: LIST iter-ergonomics
> methods.** Eight more methods on the same file-backed `native struct List[T]` seam (bodyless sigs in
> `std/prelude.chz` → name-keyed VM dispatch in `src/vm/call.rs`, appended to `LIST_METHODS`; zero new
> checker logic): `unique`/`dedup` (NEW list — first-occurrence dedup via `values_equal` vs
> consecutive-run collapse; no `where` bound, matching bound-free `contains`, so `List[float]` works),
> `chunk(n)`/`windows(n)` (`-> List[List[T]]`; `n<=0` faults `chunk/window size must be positive, got {n}`;
> `windows` `n>len` → empty; outer list rooted, inner handles pushed immediately after alloc for GC-safety),
> `take_while`/`drop_while`/`count`/`position` (predicate methods routed through `list_hof`'s snapshot+root
> discipline — a re-entrant pred that shrinks the receiver can't OOB; `position` → `Option[int]`).
> Tests: 5 inline dual-engine parity/fault tests incl. `list_predicate_shrinking_no_panic` +
> extended `examples/list_methods.chz` golden (serial==M:N). Docs: `docs/stdlib.md § List[T]`,
> `docs/gaps.md §2` (wave-2 bullet struck). **STILL OPEN §2**: `iter.min`/`max`, `group_by`/`partition`/
> `flat_map`, Map/Set ergonomics — later waves.
>
> **✅ STDLIB (2026-07-16, `auto-task/math-number-fns`) — `docs/gaps.md` §5: NUMBER/MATH surface in
> `std.math`.** Ten native fns + two float constants added to the file-backed native module (mirroring
> the existing `sqrt`/`abs`/`pi` pattern — zero new seam machinery): `gcd`/`lcm` (int; Python `math.gcd`
> semantics, `lcm` divides-before-multiplies + overflow-faults like `abs`), `sign` (numeric-poly like
> `abs`, -1/0/1), `trunc(float)->int` (toward-zero, ≡ `int(x)`), `hypot`/`cbrt` (total f64),
> `factorial`/`comb`/`perm` → `Result[int]` (clean `Err` never a fault; `factorial` ceiling `20!` = i64
> limit; `comb`/`perm` compute in i128 and Err only on true i64 overflow), and
> `parse_int_base(s, base)->Result[int]` (base 0 or 2..=36, `0x`/`0o`/`0b` prefixes, Go `strconv`-style).
> Constants `math.inf`/`math.nan` join `pi`/`e` via the existing `native_consts` path (both engines seed
> identical f64 → free parity). Pure arithmetic lives in unit-tested free helpers (`gcd_u64`/`lcm_i64`/
> `factorial_i64`/`comb_i64`/`perm_i64`/`parse_int_base_impl`); `sign` added to `MODULE_NUMERIC_POLY`.
> **`divmod` — SHIPPED 2026-07-17 (see top entry)** as a bodied Chezzi fn via the hybrid module form; no
> `NativeRet::Tuple` was needed. (Originally deferred here for lack of that seam.) Tests: `src/native/math.rs` helper units + `math_number_fns_parity`
> (graph path, serial==M:N). Docs: `docs/stdlib.md §std.math`, `docs/gaps.md §5` (SHIPPED + divmod deferral).
> **✅ STDLIB (2026-07-16, `auto-task/encoding-url-parse`) — `docs/gaps.md` §7: URL PARSING read-half in
> `std.encoding`.** Two native read-half members round out the module's existing write-half
> (`url_encode`/`url_decode`/`query_encode`): `query_decode(q: str) -> Map[str,str]` (strips a leading
> `?`, splits `&`/first-`=`, percent-decode + `+`→space, no-`=` key → `""`, DUPLICATE key = **last-wins**
> — a Map can't hold `parse_qs` lists, the Go `url.Values.Get` analog; malformed escape kept RAW, never
> faults) and `url_parse(u: str) -> Map[str,str]` (LEXICAL scheme/host/port/path/query/fragment, missing
> → `""`, components stay encoded per Python `urlsplit`/Go `net/url`, port a STRING). Both return
> `NativeRet::Map` — deliberately NOT a bespoke native struct (avoids reserved-type seeding). RUST task:
> `std.encoding` is FILE-BACKED NATIVE (all members bodyless `native fn` in `std/encoding.chz` backed by
> `src/native/encoding.rs`) — corrected the `gaps.md §7` "pure-Chezzi" mislabel. Extracted a shared
> `percent_decode(bytes, plus_as_space)` reused by `url_decode` (false) + `query_decode` (true);
> `url_decode` byte-identical after (regression test green). Tests: unit `percent_decode`/`query_decode`/
> `url_parse` + `examples/encoding.chz`/`.expected` extended (`golden_encoding_crypto_via_run_file` +
> `assert_file_parity` = serial==M:N). Docs: `docs/stdlib.md §std.encoding`, `docs/gaps.md §7` (SHIPPED +
> label fix). Full `cargo test`/`clippy`/`conformance` green.
> **✅ STDLIB (2026-07-16, `auto-task/std-log`) — `docs/gaps.md` §10: `std.log`, leveled logging.**
> NEW pure-Chezzi module `std/log.chz` over `std.io` (only Rust touch = one `include_str!` line in
> `src/resolver/std_embed.rs`, guarded by `embedded_std_table_matches_disk`). `log.new(min_level=INFO,
> to_stderr=true) -> Logger`; `debug/info/warn/error(msg)` format `"LEVEL message"` gated by
> `set_level` (Go `slog` levels `DEBUG<INFO<WARN<ERROR`, exposed as module fns `log.DEBUG()`…), written
> to **stderr** by default (Python `logging` + Go `log`/`slog` default; `to_stderr=false` → stdout).
> Timestamps opt-in/injectable via `set_prefix` — the pure deterministic `format_line(level,msg)` core
> bakes in no clock (ungoldenable), so the default path stays golden-able. Pure-Chezzi ⇒ serial-VM ==
> M:N. Tests: `parity_std_log_defaults_to_stderr` (gating+stderr routing) + `examples/log_demo.chz`/
> `.expected` (`golden_log_demo_via_run_file` pins the STDERR stream + stream-discrimination asserts +
> `assert_file_parity`). Docs: `docs/stdlib.md § std.log`, `docs/gaps.md §10` (SHIPPED). Deferred:
> handlers/formatters, hierarchical loggers, structured fields. Full `cargo test`/`clippy`/`conformance` green.
> **✅ STDLIB (2026-07-16, `auto-task/std-flag`) — `docs/gaps.md` §10: `std.flag`, a Go-`flag`-style
> CLI arg parser.** NEW pure-Chezzi module `std/flag.chz` (zero native seam — consumes the existing
> `os.args()`; only Rust touch = one `include_str!` line in `src/resolver/std_embed.rs`, guarded by
> `embedded_std_table_matches_disk`). `flag.new()` → a `FlagSet` you register `str_flag`/`bool_flag`/
> `int_flag` on, then `parse(args) -> Result[List[str]]` (`Ok(positionals)`), read back via
> `get_str`/`get_bool`/`get_int`/`positionals()`/`usage()`. Syntax: `--name value` / `--name=value` /
> bool-presence / `--` terminator; dash-insensitive lookup (`-n`==`--n`, a v1 simplification).
> Unknown/missing-value/non-int → clean `Err` (never faults); `get_*` on an unregistered name panics
> (Go-parity programmer error). Pure-Chezzi ⇒ serial-VM == M:N structurally. Tests: 4 inline
> `parity_entry` cases (value/=-form, bool+terminator, 3 error paths, deterministic usage) +
> `examples/flag_demo.chz`/`.expected` (`golden_flag_demo_via_run_file` + `assert_file_parity`).
> Docs: `docs/stdlib.md §5 std.flag`, `docs/gaps.md §10` (SHIPPED). Deferred: required flags,
> subcommands, dup-registration detection. Full `cargo test`/`clippy`/`conformance` green.
> **✅ STDLIB (2026-07-16, `auto-task/string-ergonomics`) — `docs/gaps.md` §1: STRING ERGONOMICS in
> `std.string`.** Seven pure-Chezzi free fns (zero Rust, no native method-table change), Python `str`
> semantics: `capitalize` / `title` / `swapcase` / `find(s, sub, from_index)` /
> `split(s, sep, maxsplit=-1)` / `rsplit(s, sep, maxsplit=-1)` / `split_whitespace`. `find` generalizes
> `index_of` (negative `from_index` counts from the end `len+from_index`, clamped to 0; past-end → -1; empty `sub` → clamped `from_index`) and
> `index_of` is now `find(s, sub, 0)` (behavior-preserving; `golden_str_methods` unchanged). `title`/
> `swapcase` reuse a shared `is_cased(c)` (`c.upper() != c.lower()`) helper; `split`/`rsplit` fault on
> empty `sep` (Python `ValueError`); `split_whitespace` drops empties on whitespace runs. Free-fn-only
> (NOT receiver-method aliases — no Rust seam). ASCII-guaranteed; exotic Unicode case-fold follows Rust.
> Pure-Chezzi ⇒ serial-VM == M:N structurally. Tests: `examples/str_more.chz` + `.expected` extended
> (`golden_str_more_via_run_file` + `assert_file_parity`, both engines). Docs: `docs/stdlib.md §std.string`,
> `docs/gaps.md §1` (SHIPPED). Full `cargo test`/`clippy`/`conformance` green.
> **✅ STDLIB (2026-07-16, `auto-task/datetime-parse`) — `docs/gaps.md` §9: `datetime.parse_iso8601`,
> the string→`DateTime` half (`datetime` was write-only).** Pure-Chezzi, single-file seam (NO Rust) —
> `parse_iso8601(s: str) -> Result[DateTime]`, the exact inverse of `to_iso8601`, reusing the existing
> `to_epoch`/`from_epoch`/`days_in_month` machinery. Parses ISO-8601 / RFC-3339 (matches Python
> `datetime.fromisoformat`): `"YYYY-MM-DD"` (date-only, midnight), `"YYYY-MM-DDTHH:MM:SS"`, a `'T'` **or**
> `' '` separator, an optional trailing `Z` or `±HH:MM` offset (**normalized to UTC**, per Go
> `time.Parse`), and an optional `.fff` fractional part (validated then **truncated** — `DateTime.second`
> is int). Split-based + clamped-slicing, cursor-free (json.chz style); strict local `all_digits`/
> `to_uint`/`field2` guards every fixed-width field to exactly-N ASCII digits BEFORE any conversion (never
> trusts lenient `int()`), and range-validates month/day/hour/min/sec/offset BEFORE civil math — so a
> malformed / out-of-range string is a clean `Err`, **never a fault/abort**. Round-trip `parse_iso8601(
> to_iso8601(dt)) == dt` (weekday included, rebuilt via `from_epoch`). **Known ceilings** (deliberate,
> UTC-only contract): sub-second precision dropped, non-`Z` offset normalizes to UTC not itself. Tests:
> 5 `test fn` vectors (round-trip, forms, tz, frac, 8 Err cases) + `examples/datetime.chz` parse-tour
> extended (golden `golden_datetime_via_run_file` runs serial VM **and** M:N via `assert_file_parity` —
> the checker-superset gate). **Remaining follow-up:** `strftime`/`strptime`/`from_string` (format-token
> vocabulary) deferred. Docs: `docs/stdlib.md` (§std.datetime table + ceilings), `docs/gaps.md` §9
> (parse landed, write-only de-stale'd). Full `cargo test`/`clippy`/`conformance` green.
>
> **✅ LANGUAGE (2026-07-16, `auto-task/contains-protocol`) — `docs/gaps.md` L5: the `Contains` operator
> protocol (`x in my_struct`).** A user struct/enum with a `contains(self, item) -> bool` method makes
> `x in that_value` dispatch to it, yielding `bool` (Python's `__contains__`, Go's idiom). Registered
> `Contains[Item]` as a reserved operator protocol mirroring `Index[K,V]` — 4 drift-locked sites
> (`is_reserved_protocol` + `prebuilt_protocols` seed + `assert_native_protocol_shape_matches` array +
> `std/prelude.chz`). The `in` operator recovers the item type directly via a new `contains_item_ty`
> helper (modeled on `index_kv`: struct + enum arms, arity==2 + `ret==bool` shape gate,
> `struct_param_map`/`enum_param_map` generic subst so `Box[int]`'s item is `int` not `Param(T)`), then
> checks LHS↔item compatibility. **NO new opcode / compiler change:** `op_contains` (vm/arith.rs) peeks
> `matches!(Obj::Struct|Obj::Enum)` (ends the heap borrow) then dispatches via `resolve_overload_method` +
> `guarded(run_proto)` — the `struct_compare` template — validating the result is `Value::Bool`. Single
> VM change covers cooperative + M:N. Container `in` (list/set/map/str) is byte-identical. Clean checker
> errors (never runtime panics) for item-type mismatch (`"s" in bag_of_int`), a missing/wrong-return
> `contains` (hint names `contains(self, item) -> bool`), and generic subst. Tests: 3 dual-engine RUN
> (`assert_mc_parity`: struct true/false, generic `Box[int]`, enum), 5 checker guards, +
> `examples/contains_protocol.chz` golden (`golden_contains_protocol_via_run_file` + `assert_file_parity`).
> Newtypes deliberately out of scope (consistent with the proto.rs method-operator-on-newtype carve-out).
> Docs: `docs/syntax.md §7b`, `docs/spec.md`, `docs/gaps.md` L5 (FIXED). Full `cargo test`/`clippy`/`conformance` green.
>
> **✅ LANGUAGE (2026-07-15, `auto-task/struct-match-patterns`) — `docs/gaps.md` L2: STRUCT PATTERNS in
> `match` (positional field destructuring).** `match p: Point(x, y):` now binds a struct's fields
> positionally, mirroring enum-variant patterns — closing the "enums destructure, structs don't"
> asymmetry. A struct has exactly ONE constructor, so a lone all-binding `Point(x, y)` arm is
> irrefutable ⇒ exhaustive with **no `_`**. Nested (`Line(Point(x, y), _)`), generic (`Box(v)` on
> `Box[int]` binds `v: int`, instantiated not `Unknown`), literal fields (`Point(0, y)` — refutable,
> needs `_`/catch-all), and a whole-value catch-all binding (`rest:`) all work. **Both spellings:** BARE
> `Point(x, y)` (local / `from`-imported struct) AND QUALIFIED `geo.Point(x, y)` (the only spelling for a
> WHOLE-module-imported struct — the bare name isn't in scope — symmetric with qualified construction).
> Arity mismatch, wrong constructor, an enum-name-collision qualifier (`E.Point`), a non-module qualifier,
> and a 3-part path are clean **checker** errors, never runtime panics; a DUPLICATE constructor arm
> (`Point(x,y): … / Point(a,b): …`) is now a `duplicate match arm` error like enum/literal arms. Only
> **user** structs (`StructOrigin::User`) destructure — a native handle (Socket/Ref/`regex.Match`) stays
> non-destructurable, so the checker never accepts a pattern the compiler can't lower. **Reused the
> enum-variant `Pattern::Variant` node — NO new AST node / opcode / parser change.** Seams:
> `MatchKind::Struct` (checker/mod.rs) + `match_kind` Ty::Struct arm + `struct_fields_of` (checker/sig.rs,
> user-gated); Struct arms in `bind_match_arm` / `bind_subpattern` / `check_exhaustive` + the shared
> `resolve_struct_ctor` (checker/pattern.rs — bare via `bare_key`, qualified via `imported_modules`+
> `type_key`, unifying both arms); `struct_key_of_pattern` + `pattern_needs_enum` (refined `EnsureEnum`
> guard so a struct-only match doesn't emit it and fault — the checker-superset trap) + the `emit_pattern`
> struct branch emitting `GetField{name}` per field (compiler/mod.rs). Tests: dual-engine RUN
> (`struct_match_binds_fields` / `_generic_and_nested` / `_literal_field_refutable` /
> `_qualified_whole_module_runs_both_engines` via `assert_mc_parity` / `run_file`+`run_file_p` — the
> load-bearing check the parity oracle is blind to), 11 checker guards, `examples/match_struct.chz`
> golden (+ picked up by `all_shipped_examples_typecheck`, the real `check_graph` path). **Still
> deferred:** `let Point(x, y) = p` (Let carries `names`, not a `Pattern` — separate seam) + fn-param
> destructuring. Docs: `docs/syntax.md §8`, `docs/spec.md`, `docs/grammar.bnf` (prose — productions
> already admitted `Name(subpatterns)`), `docs/gaps.md` L2. Full `cargo test`/`clippy`/`conformance` green.
>
> **✅ LANG+STDLIB (2026-07-15, `auto-task/native-struct-bodied-methods`) — bodied Chezzi methods on a
> `native struct`, first user `Reader.lines() -> Iterator[str]` (`docs/gaps.md` R2b `lines()` DONE).**
> A `native struct` (a RESERVED opaque VM handle — `Ty::Reader`/`Writer`/…, no `StructDef`/`tid`) may now
> MIX Rust-backed bodyless `native fn` sigs (dispatch stays native, name-keyed) with pure-Chezzi bodied
> `fn` methods (COMPILED to bytecode, routed via new `Program::native_methods`/`native_home`, keyed by
> the handle's bare reserved name — the SAME type-erased, generator-aware mechanism as `enum_methods`).
> Shipped `fn lines(self) -> Iterator[str]` in `std/io.chz` (a generator over `read_line()`): `for ln in
> r.lines():` streams **lazily** — the file is not snapshotted, an early `break` stops reading (Python
> `for l in f` / Go `bufio.Scanner` / Rust `BufRead::lines`). `read_line`/`read_bytes`/`close` are NOT in
> `native_methods`, so they stay byte-identical. Seams: parser accepts a bodied `fn` in a native-struct
> body into a new `bodied_methods: Vec<FnDecl>` (bodyless `native fn` + `test fn`-reject unchanged);
> checker harvest (`setup.rs` PASS 1b) folds them into the handle's method table via `fn_sig` (leading
> `self` stripped to match); compiler adds a native-struct bodied-method pass mirroring the enum pass; VM
> `do_method_call` Reader arm tries `try_native_bodied_method("Reader", …)` before `reader_method`.
> **Known v1 limit:** the bodied body is compiled-but-NOT-type-checked (the native module skips
> `check_module`), so the mandated dual-engine RUN test is the safety net for any future bodied method.
> Spawn-across-airlock for a generator holding a native handle: noted, not exercised (the handle itself is
> sendability-tested). New tests: `reader_lines_parity` + `reader_lines_lazy_early_break_parity` (both
> engines) + parser unit tests. Docs: `docs/stdlib.md` (Reader.lines), `docs/gaps.md` (R2b + IO §4 flip).
> Full `cargo test`/`clippy`/`conformance` green.
>
> **✅ STDLIB (2026-07-15, `auto-task/stdin-read-all-char`) — `docs/gaps.md` R2 stdin grab-bag DONE:
> `io.read_all()` + `io.read_char()`.** Two module-level stdin readers in `std.io`, plain name-dispatched
> natives (siblings of `read_line`, NOT engine-intercepted), routed through the SAME shared `Stdin` seam
> so they inherit shared-stdin / no-new-false-EOF task behavior for free. `read_all() -> str` drains ALL
> remaining stdin to EOF as one `str` (Python `sys.stdin.read()`; `""` at clean EOF) — a bare `str`, not
> `Result`/`Option`: stdin carries no open-error and `None`-vs-`""` is meaningless for a drain. `read_char()
> -> Option[str]` reads ONE Unicode scalar as a 1-char `str` (Chezzi has no `char` scalar; `None` at clean
> EOF). Both **fault** on non-UTF-8 (`"stdin: stream is not valid UTF-8"`) — there is no stdin `read_bytes`
> hatch, so the message stands alone. Touch points: `Stdin::read_all`/`read_char` on all 3 variants
> (`src/native/mod.rs` — Empty→EOF-equiv, injected `Lines`→line+`\n` reconstruction / front-char drain,
> `Real`→`read_to_string` / raw-byte scalar read off the process-global `stdin()`); `Host` trait defaults
> (EOF-equivalent `Ok("")`/`Ok(None)`, so the ~7 MockHosts need no edits); `VmHost` delegates, `OffloadHost`
> `unreachable!` stubs; `io.rs` natives + MEMBERS (15→17); `std/io.chz` decls. NOT added to `is_blocking`
> (host-stdio, runs inline like `read_line`; an offload would hit `OffloadHost`'s stdio `unreachable!`) —
> so they inherit the accepted v1 "blocked reader pins an M:N worker" limit; the task-stdin false-EOF drift
> is untouched (out of scope). 2 parity tests (single entry task → exact `assert_eq`: drain + scalar-by-
> scalar-then-`None`) + 2 real-process `tests/interactive.rs` tests (byte-exact multibyte over `Stdin::Real`,
> which the injected `Lines` model can't observe), each mn+serial. Ratchet `std.io` members 15→17. Docs:
> `docs/stdlib.md` (table + input-contract + v1-limit), `docs/gaps.md` bullet closed.

> **✅ STDLIB (2026-07-15, `auto-task/fs-trio`) — `docs/gaps.md` fs grab-bag DONE: `canonicalize` +
> `chmod` + `atomic_write` in `std.fs`.** Three pure blocking-OS natives in `src/native/fs.rs`, each
> mirroring an existing fs native's idiom. `canonicalize(path) -> Result[str]` resolves symlinks +
> `.`/`..` against the REAL filesystem to an absolute real path (requires the path to EXIST — distinct
> from the purely-lexical `path.normalize`). `chmod(path, mode: int) -> Result[nil]` sets unix
> permission bits via `set_permissions`/`PermissionsExt::from_mode`, `#[cfg(unix)]`-gated (non-unix arm
> `Err`s `"chmod is unix-only"`); mode passed unmasked to the OS. `atomic_write(path, contents) ->
> Result[nil]` writes a temp file in the SAME dir as the target then `rename`s over it (atomic within
> one filesystem — a `/tmp` temp would break atomicity; a per-write pid+seq temp name avoids collision).
> All three are in `native::is_blocking` (+ the offloadable-set test) so the M:N engine offloads them to
> the dirty pool + fires the cancel checkpoint; `chmod`'s int mode crosses the off-heap boundary via
> `NativeArg::Int`. Two-engine parity is automatic (pure blocking native, no scheduler). 3 new unit
> tests (canonicalize resolves a real symlink + errs on nonexistent; chmod sets then metadata-read
> confirms 0o644/0o600; atomic_write writes/overwrites + leaves exactly one entry — no stray temp).
> Registered in `std/fs.chz` decls. Docs: `docs/stdlib.md` (§std.fs surface), `docs/gaps.md` (fs
> grab-bag SHIPPED + metadata-READ still-missing note). Full `cargo test`/`clippy` green.
>
> **✅ STDLIB (2026-07-15, `auto-task/std-io-reader`) — `docs/gaps.md` R2b DONE: a read-only `Reader` /
> file-handle type in `std.io`, the read twin of R2's `Writer`.** Line/chunk streaming of a large file
> (past the 64 MB whole-file `read_file`/`read_bytes` cap). Opener `open(path)`, methods
> `read_line()` / `read_bytes(n)` / `close()`. `read_line() -> Option[str]` streams one line at a time
> (trailing `\n`/`\r\n`/bare-`\r` stripped, `None` = EOF) — matching the module-level `read_line()` shape
> (anti-drift); a mid-read I/O error or non-UTF-8 file is a clean runtime **fault** pointing at
> `read_bytes` (an `Option` can't carry the error, mirroring `read_file`). `read_bytes(n) -> Result[bytes]`
> is the binary + error-distinguishing escape hatch (at-most-n bytes, empty = EOF, `Err` on closed/IO).
> `close()` idempotent (fd closes on `BufReader` drop — no `Drop` impl, reads are flush-free). Modeled
> arm-for-arm on `Writer`: `ReaderCore { Mutex<Option<BufReader<File>>>, key }` in `src/vm/core.rs`,
> `Obj::Reader`/`WireValue::Reader` (GC leaf, sendable across the airlock), methods + opener in
> `src/vm/fileio.rs` (blocking-classified, NO netpoller, NO `stream_halt` — a Reader never emits),
> `Ty::Reader` gated by `import std.io` (3-touchpoint additive checker seam + `io_reader_seed` field,
> SEPARATE from `io_writer_seed`). Cross-task read order to one shared handle is unspecified (offset race)
> — no shared-read parity test (meaningless); sendability tested via send-across-`spawn`. (`lines() ->
> Iterator[str]` since SHIPPED as a bodied Chezzi method — see the top entry.) 11 new tests (7 run-parity
> both engines incl. bare-`\r` strip + 2 checker-level guarding the
> `Ty::Reader` method arm + the reserved-name ratchet + the io member-count bump). Docs: `docs/stdlib.md`
> (Reader surface), `docs/gaps.md` (R2b DONE + IO §4 refreshed). Full `cargo test`/`clippy` green.
>
> **✅ STDLIB (2026-07-15, `auto-task/request-get-bytes`) — R2 follow-up DONE: binary HTTP download in
> `std.request`.** New `request.get_bytes(url, timeout_ms?) -> Result[bytes]` reads the body via
> `into_reader().read_to_end` → the same immutable `bytes` value `Socket.read_bytes`/`io.read_bytes`
> return, so an image/zip/pdf round-trips byte-exactly instead of `into_string()`'s `from_utf8_lossy`
> corruption. GET-only + body-only: a non-2xx status is an `Err` (a 404/500 error page can't pose as a
> successful download — `io.read_bytes` semantics), headers dropped; 64MB download cap. Text
> `get`/`post` path unchanged. `get_bytes` is in `native::is_blocking` so the M:N engine offloads it to
> the dirty pool (never pins a core worker) + the cancel checkpoint fires before it — same as every
> other request verb. Parity is automatic (blocking ureq native, not netio-gated). 5 new unit tests
> (byte-exact, corruption-contrast, truncated→Err, 404→Err, every-request-member-is-blocking).
> `docs/gaps.md:153` closed, `docs/stdlib.md` binary-download note. Full `cargo test`/`clippy` green.
>
> **✅ STDLIB (2026-07-15, `auto-task/writer-r2`) — `docs/gaps.md` R2 DONE: a write-only `Writer` /
> file-handle type in `std.io`.** Buffered + streaming write output, the escape hatch Chezzi's unbuffered
> stdout default was missing. Openers `create` (truncate) / `append` (create-if-absent), stream handles
> `stdout()`/`stderr()` (routing through the same `Vm::emit_out`/`emit_err` sink as `print` — never a raw
> fd, so capture/parity/streaming hold), a `buffered(w, size = 8192)` wrapper (Go's `bufio.NewWriter`:
> one host/fd write per `flush`/buffer-full/`close`), and methods `write`/`write_bytes`/`flush`/`close`.
> Modeled arm-for-arm on the `Socket` native handle: `WriterCore { Mutex<Option<Backing>>, key }` outside
> every heap (`src/vm/core.rs`), `Obj::Writer`/`WireValue::Writer` (GC leaf, sendable across the airlock),
> methods + openers in the new `src/vm/fileio.rs` (blocking-classified, NO netpoller — files are always
> epoll-ready), func-pointer intercept in `invoke_native` (the `append` opener collides with `fs.append`'s
> bare name; only the fn-ptr distinguishes them), `Ty::Writer` gated by `import std.io`. Use-after-close =
> clean `Err` (the `Mutex<Option>`); a buffered **file** writer flushes its tail best-effort on drop
> (buffered stdout/stderr can't reach `&mut Vm` from Drop — needs explicit `flush()`/`close()`, a
> documented ceiling). Cross-task write order to one shared handle is unspecified (Go's bufio rule) — its
> parity test uses `assert_same_lines`; single-task buffered stdout is byte-identical serial vs M:N.
> `fs.append(path, text)` UNTOUCHED (no collision — `std.io` owns the handle verbs). 15 new tests (11
> run-parity + 2 checker-level guarding the `Ty::Writer` method arm that `run_file` can't see, + the
> reserved-name ratchet + the native uniqueness exemption). Docs: `docs/stdlib.md` (Writer surface),
> `docs/gaps.md` (R2 retired + IO §4 refreshed). Full `cargo test`/`clippy` green.
>
> **✅ LANGUAGE SEMANTICS + FIX (2026-07-14, `auto-task/cancel-points`) — CANCELLATION POINTS; `docs/gaps.md`
> N6 FIXED, N4's "defers now always run" overclaim corrected.** A cancel (sibling fault, `os.exit`, scope
> teardown) is now delivered at **checkpoints — loop back-edges + blocking/park ops** — *not* at every
> instruction (the loop-top check in `run_until` is **deleted**; the back-edge check is `Vm::jump_checked`,
> wired into BOTH `Op::Jump` dispatch sites, and the park checkpoints are engine-agnostic top-of-fn checks
> in `chan_recv_step` / `op_wait_poll` / `park_on_fd` / the blocking-native offload). **Therefore a STARTED
> task always runs its straight-line prologue, so a REGISTERED `defer` ALWAYS runs on cancel — on BOTH
> engines, deterministically.** Before: a task could be killed *between its first statement and its `defer`
> line* — the probe shape ran its defer in **0/20** M:N runs. This is Trio's model (the old
> every-instruction kill was neither Go's nor Trio's); accepted cost: a cancelled task runs to its next
> checkpoint, and at a `recv`/`wait:` checkpoint **cancel now wins over a queued value / done-latch / fired
> timer** (uniform on both engines). Second, independent fix (**N6**): serial's `run_scheduler` used to
> propagate a faulting child's error straight out with `run_child(i)?`, **abandoning its still-parked
> siblings** — it now trips a scope cancel and **re-drives every not-`Done` sibling** to completion
> (`drain_cancelled_children`) so each unwinds its `defer`s before the fault propagates, reducing exits like
> M:N (`Exit` > `Fault`, lowest index — an `os.exit` from a drained child's `defer` is carried, never
> dropped). M:N's `TaskOutcome::Cancelled` now **carries + flushes its output** (those lines really printed;
> serial can't un-print them). **Rule, documented:** cross-task stdout ORDER is nondeterministic on both
> engines and is NOT parity — the **line set**, the **exit code** and **whether the defer ran** are.
> Race bar (release, N-1 CPU load hogs): defer-first / probe / token shapes = `42`, **0/200 failures per
> engine per shape** (before: probe 0/20 defers on M:N, token serial `0` 10/10). New parity tests:
> `parity_defer_runs_on_parked_sibling_when_sibling_faults`,
> `parity_probe_defer_runs_when_cancelled_before_its_defer_line`,
> `parity_os_exit_inside_a_cancelled_tasks_defer`; plus
> `parallel_spinning_sibling_does_not_hang_the_nursery_under_cancel` (a `while true:` sibling still dies at
> the back-edge) and `compiler::back_edge_tests::loop_back_edge_is_a_backward_jump` (peephole may never
> thread a loop back-edge away, or hot loops become uncancellable). **N5 untouched** (a genuine deadlock
> still skips defers, identically on both engines). Perf side-effect of
> deleting the per-instruction check: `loop` 1.32× → **1.12×** CPython, `fib` 3.54× → **3.16×**
> (`docs/benchmarks.md`).
>
> **Post-review corrections (same branch — the first cut shipped two of these as bugs).**
> (a) **`gaps.md` N6b — every spawned task STARTS, even into an already-cancelled scope.** M:N is
> structurally forced to (a scope completes only at `done == total`; `take_runnable` never checks the
> scope cancel), so the drain's original "skip never-started siblings" made serial print `{"0"}` where
> M:N printed `{"hi","42"}`, **20/20** — a deterministic line-SET + defer-ran divergence. The drain now
> re-drives **every not-`Done`** sibling. `exit_in_spawned_child_aborts_siblings`'s serial golden moved
> deliberately (`{"a"}` → `{"a","b"}`): M:N already printed both **20/20**, so the engines converged.
> New tests: `parity_probe_faulter_spawned_first_still_runs_the_siblings_defer`,
> `parity_straight_line_sibling_runs_even_when_the_scope_is_already_cancelled`.
> (b) **`gaps.md` N6c — a native HOF's per-element callback IS a loop back-edge.** `map`/`filter`/`fold`/
> `sort(cmp)` iterate in Rust and emit no `Op::Jump`, so a cancelled task burned every element to
> completion (measured: 5M callbacks after the sibling faulted). The checkpoint now sits at the top of
> `Vm::guarded` — one choke point, off the bytecode hot path. Test: `parity_native_hof_loop_is_cancellable`.
> (c) **LOUD, accepted limit: loop-free RECURSION is not a cancellation point** (only `Call`/`Return`, no
> back-edge) — a cancelled task inside `fib(34)` finishes it before it dies. Making `Op::Call` a
> checkpoint would re-open BUG 1 (a checkpoint before the `defer` line of any prologue that calls a fn).
> Both engines agree, so it is a limit, not a divergence. Also covered: `parity_nested_deadlock_cancels_the_outer_parked_siblings_defer`
> and `parity_genuine_deadlock_is_still_detected` (N5 boundary intact). Race bar re-run after the fixes:
> 4 shapes × 2 engines × **200 runs under full CPU load = 0 failures**.

> **Post-review round 2 (same branch) — three MORE confirmed bugs in the first cut, all fixed
> (`docs/gaps.md` N6d/N6e/N6f), each RED-first.**
> (d) **A `defer` was itself cancelled.** Every deferred call runs through `Vm::guarded`, whose new
> checkpoint fired on the FIRST (LIFO) defer of any task that returned normally / faulted on its own
> under an already-tripped scope cancel (`cancelled` still false there) — the defer body never ran.
> Silent PARTIAL cleanup, deterministic, both engines (so parity stayed green). Fixed with `Vm::deferring`
> + the single cancel predicate `Vm::cancel_requested`: **no cancellation point fires inside a `defer`**.
> (e) **A nested `parallel:` inside a cancelled task was uncancellable → the teardown HUNG** (new hang,
> both engines). Cancelling a scope must cancel its descendants: nested scopes now inherit the enclosing
> cancel flags (`JoinScope::ancestors` → `Vm::cancel_outer`; serial inherits the `Arc`), while keeping
> their own token so an inner fault never cancels an outer sibling.
> (f) **The blocking-op checkpoint was inside the `mn.is_some()` gate → serial had none**: a cancelled
> serial task slept out its `sleep_ms` (stalling the whole teardown) and then ran every statement after
> it — line-SET *and* exit-code divergence vs M:N. Moved outside the gate (same for the socket park), so
> the checkpoint SET is engine-agnostic as the contract claims.
> New parity tests: `parity_every_defer_of_a_normally_returning_task_runs_under_a_tripped_cancel`,
> `parity_nested_nursery_inside_a_cancelled_task_is_cancellable`,
> `parity_blocking_native_is_a_cancellation_checkpoint_on_both_engines`. Race bar re-run: 3 shapes × 2
> engines × **200 runs under CPU load = 0 failures** (before, on `main`: serial 0/50 on all three shapes;
> M:N probe 0/50). Full `cargo test` green ×2, clippy clean, benches unmoved.

> **Post-review round 3 (same branch) — a `defer` whose body BLOCKS (`docs/gaps.md` N6g), both M:N-only,
> both RED-first.** (g1) The M:N demote loops read the raw `self.cancel` flag instead of
> `Vm::cancel_requested()`, so a blocking op *inside* cleanup (a `sleep`, a `sock.close()`, a final
> `send`) saw the already-tripped scope cancel and **truncated the defer mid-body** — `CLEANUP-ENTER`,
> then nothing: sentinel `0` on M:N vs `42` on serial. (g2) With that fixed, a `defer` that can **never**
> complete (`recv` nobody will answer) correctly cannot be cancelled out — and then **hung M:N silently**:
> `demote_recv_block`'s deadlock self-detect was vetoed forever by N4's `any_incomplete_scope_cancelled`,
> whose liveness argument ("a cancelled scope always reaches `done == total`") a never-completing defer
> falsifies (M:N rc=124 hang vs serial rc=1 report). The veto is now **bounded to the trip→`cancel_drain`
> window** it exists for — an *undrained parked* fiber of the cancelled scope
> (`SchedCore::any_cancelled_scope_awaiting_drain` + `scope_has_undrained_park`) — so the quiesce fires,
> the stuck cleanup is reported as a deadlock, and the sibling's real fault propagates: same line set on
> both engines. **The rule (now in `docs/concurrency.md`):** cleanup that blocks on time/IO is
> uninterruptible and delays the teardown by exactly that long, no cap (Go's deferred-fn-during-panic
> rule); cleanup that can never complete is REPORTED as a deadlock, never a silent hang. New tests:
> `mnsched_cancelled_scope_whose_only_fiber_is_demoted_is_deadlock`,
> `mnsched_cancelled_scope_with_a_parked_and_a_demoted_fiber_is_not_deadlock` (N4 boundary),
> `parity_a_blocking_defer_body_completes_when_the_task_is_cancelled`,
> `parity_a_defer_that_can_never_complete_is_reported_not_hung` (hard 20 s deadline — a hang FAILS).
> Race bar: 2 new shapes × 2 engines × **200 runs under full CPU load = 0 failures** (before: the
> forever-defer hung **200/200** on M:N).

> **Post-review round 4 (same branch) — the bounded N4 veto lost the DEMOTED half (`docs/gaps.md` N6g.3),
> RED-first.** Bounding the veto to "an undrained PARKED fiber of a cancelled scope" was too narrow in the
> other direction: a fiber demoted (`blocked_native` — a `recv` reached inside a native HOF callback /
> `Shared.update` / an `Executor` handler) is not in `parked`, yet a **cancel WILL wake it**
> (`demote_recv_block` ranks `cancel_requested()` above `terminate` and above its own deadlock
> self-detect), after which it unwinds and runs its `defer`s — which can `send`. Cancel is a wakeup source
> the `running`/`runnable`/`inflight`/`parked` counters do not model, so an idle worker could declare a
> **spurious deadlock** in the ≤5 ms `DEMOTE_POLL_BACKOFF` window before that fiber saw the cancel;
> `flag_deadlock` then reaps every parked fiber of every scope *without* `unwind_deferred` (the exact N4
> lost-defer symptom) and latches `terminate`, truncating a sibling's in-flight cleanup. **Fix:** a demoted
> fiber now WATCHES the cancel flags it would honour (`Vm::demote_cancel_flags` →
> `SchedCore::watch_demoted_cancel`, dropped on every demote-loop exit), and `is_deadlocked` vetoes while
> one is tripped (`any_demoted_cancel_pending`). The watch is EMPTY for a fiber a cancel can never wake —
> already unwinding, or blocked inside its own `defer` (`deferring > 0`) — so the never-completing cleanup
> of round 3 still fires as a real deadlock, no hang re-introduced. Both vetoes now run *after* the counter
> gate (only at a candidate quiesce), keeping the `parked` scan off the idle/steal hot path. New test:
> `mnsched_demoted_fiber_with_a_tripped_cancel_is_not_deadlock`. Also corrected an overclaim: a fiber
> already **parked inside a NESTED nursery** when the outer scope is cancelled does not run its `defer`s on
> **either** engine (the drain is scope-scoped; N5 family, both engines agree — `docs/gaps.md` N6g "out of
> scope"). Race bar re-run: `defer_blocking` × 2 engines × **200 runs under 8-way CPU load = 0 failures**;
> full `cargo test` green ×2, clippy clean.

> **Post-review round 5 (same branch) — the cleanup's own SPAWNED work was still being killed
> (`docs/gaps.md` N6h), RED-first.** The `deferring > 0` suppression that makes a `defer` uncancellable is
> **per-`Vm`** and does not cross the airlock — a worker fiber is a fresh `Vm` with `deferring == 0` —
> while the cancel-flag CHAIN does (`Vm::scope_ancestors` → `JoinScope::ancestors` → `cancel_outer`). So a
> `parallel:`/`spawn` opened by a cancelled task's cleanup inherited the already-tripped enclosing flag and
> its children died at their first checkpoint: M:N `CLEANUP-ENTER|CLEANUP-DONE|sentinel=0` (rc 0, silent)
> vs serial `sentinel=42` — deterministic, and a **regression vs `main`** (serial severs the cancel in a
> defer: `run_scheduler`'s `in_defer`). **Fix:** `Vm::scope_ancestors` severs identically (empty chain while
> `deferring > 0`) — cleanup that *delegates* is now as uncancellable as cleanup that blocks inline, and the
> defer's nursery still gets its own fresh flag for its own faults. Also: the N4 demoted-veto got the
> **program-level** test it lacked (`parity_a_cancel_wakeable_demoted_fiber_is_not_a_deadlock` — a `recv`
> inside a `map` callback + a faulting sibling + an innocent parked outer sibling; **7/8 RED** with the veto
> removed), and `cancel_requested` / `demote_cancel_flags` now share ONE flag set + ONE suppression
> predicate (`Vm::cancel_flags` / `Vm::cancel_suppressed`) instead of a hand-copied duplicate that could
> drift. **Corrected overclaim (`docs/gaps.md` N6g):** the residual serial/M:N defer-blocking divergence is
> **not** "message-only" — a `defer` that `recv`s a value a LIVE sibling will send cannot park on `--serial`
> (a defer body runs guarded; the **C5** no-park-inside-native limit), so serial faults it in place while
> M:N demotes and completes. Outcome-level, recorded as OPEN N6g, pinned by
> `c5_limit_a_defer_that_recvs_from_a_live_sibling_cannot_park_on_serial`; lifting it is C5 work, not
> cancellation work. Race bar: nursery-in-defer + `defer_blocking` × 2 engines × **200 runs under 8-way CPU
> load = 0 failures**; full `cargo test` green ×2, clippy clean.

> **✅ PACKAGING (2026-07-14, `auto-task/std-embed-repl-drop`) — `docs/gaps.md` T1 FIXED: an installed
> `chezzi` now carries its own stdlib.** `std/**/*.chz` is `include_str!`'d into the binary (new
> `src/resolver/std_embed.rs`: a flat `STD_FILES` table + `lookup`, mirroring the existing `docs/*.md`
> embedding), and *every* `std.*` source read — `Builder::visit` (incl. the always-linked
> `std.prelude`/`std.ref`) and `Builder::visit_native_file` (the file-backed natives `math`/`regex`/`io`/…)
> — routes through the new **`resolver::std_source(dotted)`**: **`$CHEZZI_STD` (dev override, exclusive:
> a module absent from it is a hard error, never a silent fall-back) → the embedded stdlib.** The
> build-time `env!("CARGO_MANIFEST_DIR")/std` path is out of the READ chain, so `cargo install --path .`
> now yields a binary that survives the checkout being moved or deleted (E2E-verified with `mv std
> std.bak`: both engines, `run` and `run --serial`, byte-identical output). `std_root()` itself is
> unchanged — it still supplies ModuleId paths, diagnostics and `is_std`'s entry backstop; only the TEXT
> source moved. Bonus: a missing std module reports *"no such module in the stdlib"* rather than leaking
> the build machine's path. The hand-written table is rot-guarded by `embedded_std_table_matches_disk`
> (embedded key set **and** contents == the on-disk `std/` tree) — **add a new `std/foo.chz` and that test
> fails until you add its `include_str!` line.** Known delta: a *pre-built* binary + an edited `std/*.chz`
> is stale until rebuilt (`cargo run`/`cargo test` rebuild automatically; else use `CHEZZI_STD=./std`).

> **✅ CLI (2026-07-14, same branch) — `docs/gaps.md` T2 FIXED: `chezzi repl` de-advertised.** The stub
> arm and its USAGE line are deleted, so `chezzi repl` is now a plain unknown command (prints USAGE, exits
> 1). **No REPL was ever built and none was built here** — `docs/spec.md`'s M1 row no longer claims one
> shipped. The idea stays parked in `docs/future.md` (Tier 4, Ecosystem) as explicitly unbuilt. The CLI now
> ships exactly the 8 commands it documents: `init run test check tokens ast docs help`.

> **Mode:** Claude implements directly — working, tested code each session (see `CLAUDE.md`).
> Full per-milestone detail lives in git history; this file is a forward-looking tracker, not a changelog.

> **Gap backlog:** [`docs/gaps.md`](docs/gaps.md) — catch-all backlog; currently ranked stdlib
> depth/ergonomics gaps (string format-spec, list/iter helpers, lazy itertools, file handles, …) +
> dependency-bump notes. Draw from it when a feature earns its own milestone.

> **✅ SOUNDNESS FIX + BREAKING LANGUAGE CHANGE — `int`→`float` widening is now UNTYPED-CONSTANT-only
> (2026-07-13, `auto-task/float-widen-untyped-const`).** A value whose STATIC type was `float` could be a
> runtime `Int` — a pre-JIT-freeze blocker (a JIT emits an `f64` load over an int payload). ROOT CAUSE: the
> CHECKER widened an int/float mix to `float` at collection-element and scalar sinks, but the COMPILER is
> **type-blind** (`compile_graph` sees only the AST) and coerced only two SYNTACTIC subsets — an annotated
> `let` hint, and an all-literal peephole — so every other path left the `Int` in place. Reproduced (both
> engines, byte-identical, parity blind to it): `a := 1; f([a, 2.5])` into a `List[float]` param → `0.0`
> not `0.5`; `f := 2.5; xs := [1, f]` → `0`; a `Map[str, float]` value; a `-> List[float]` return; a
> `List[float]` field; across a `Channel`. Sharpest proofs: a `float`-typed value raising **integer
> overflow** (floats saturate), and `.sort()` on a `List[float]` returning an **unsorted** list.
> **FIX (checker-only narrowing; the compiler stays type-blind):** adopt Go's rule — an untyped int
> **constant** adapts to a float context; a **typed** int value never implicitly converts (write
> `float(x)`). One shared predicate, `ast::const_num` / `ast::untyped_int_const` (`src/ast/mod.rs`: int
> literal / unary `-` / `+ - * / %` over those; a PREDICATE, no i64 folding ⇒ no new overflow semantics),
> is called by BOTH the checker (which sinks may widen) and the compiler (which expressions get
> `Op::CoerceFloat`), so the checker's accepted set is a **subset** of what the backend can lower, BY
> CONSTRUCTION. The 8 `assignable_w(.., widen)` call sites now pass `untyped_int_const(expr)` instead of a
> blanket `true`; collection widening (`infer_list` / `infer_map`-value, replacing `numeric_mix`) fires only
> when the compiler is guaranteed to coerce — an untyped-float-constant sibling (its peephole) **or** the
> annotated-`let` element hint (`ast::ElemFloatHint`, moved out of the compiler and now shared, `take()`-cleared
> at `infer_kind` exactly like `compile_expr` so a nested literal/call arg cannot inherit the license) — AND
> every int item is an untyped constant. `Op::CoerceFloat` and every emit site STAY (the callee-side
> `emit_float_param_prologue` is load-bearing for fn-values/closures/methods). Also fixes two *pre-existing*
> unreported leaks the old literal-only peephole missed: `[1, -2.5]` and `[1, 2.0 + 0.5]` (float sibling is
> Unary/Binary ⇒ never fired ⇒ `Int` under float; now `0.5`). `Host::arg_float` keeps its runtime int
> leniency as defence-in-depth. **BREAKING** (each now a clean error naming the fix — `a typed int never
> widens to float — write float(x)`): `i := 1; x: float = i`, `x: float = i + 1`, `x: float = cmp.max(1, 2)`
> (a fn RESULT is a typed int, even with constant args — correct Go), `f(a)` into a `float` param,
> `fn g(n: int) -> float: return n + 1`, `math.sqrt(i)`, `a := 1; xs: List[float] = [a, 2.3]`, and the
> un-annotated `xs := [1, f]`. Everything untyped-constant still adapts (`x: float = 1 + 2` → `3.0`, `P(3)`,
> `fn g(a: float = 3)`, `[1, 2.3]`, `xs: List[float] = [1, f]`). In-repo blast radius: **1 `.chz` file**
> (`tests/corpus/accept/struct_methods.chz` — `-> float: return self.x * self.x + …` with `int` fields →
> `float(...)`). Tests: +11 checker (V1/V2/V3, `-> List[float]` return, `List[float]` field,
> `Channel[List[float]]` airlock, both PROOF programs, the typed-int scalar sinks, an ADAPTS
> over-rejection guard, a hint-leak guard) + 3 new/4 rewritten two-engine parity RUN tests; 4 pinned tests
> FLIPPED to rejections and 1 (which pinned the un-annotated leak AS behavior) deleted. `widen_three_engines`
> lost its stale "interp" label (it called the M:N engine twice). No VM/opcode change ⇒ no perf delta.
> Docs: `docs/syntax.md §3`, `docs/spec.md`, `docs/stdlib.md`, `docs/future.md`, `gaps.md`.
>
> **Follow-up (same branch, adversarial review): the four sinks the first cut still leaked.** The
> "checker ⊆ compiler BY CONSTRUCTION" claim was only true where the compiler's coercion site actually
> exists — it does not for a generic-ERASED callee, and it did not RESOLVE a `float` spelled through an
> alias. (1) **Function-VALUE calls no longer widen** (`checker/expr.rs`, both the positional and the
> keyword path): `fn id[T](x: T) -> T` + `f := id[float]` + `f(1)` checked clean and landed an `Int` under
> a static `float` (`f(1) / 2` → `0`; a `List[float]` built from it sorted UNSORTED), because the callee
> prologue keys on the DECLARED param (`T`) — and a `Ty::Func` cannot be told apart from a plain
> `fn(float)`. Write `f(1.0)`. (2) **Alias transparency in the BACKEND** (`compiler/mod.rs`, new
> `FloatAliases`, built per module graph before hoist): every compiler coercion site tests the SYNTACTIC
> `ast::is_float_ty` (`name == "float"`), which a `type F = float` alias never matched, while the checker's
> sink is the RESOLVED `Ty::Float` — so `x: F = 1`, `fn g(z: F)`, `-> F`, a `v: F` field, `List[F]` all
> checked clean and lowered with NO `Op::CoerceFloat` (`x / 2` → `0`; the integer-overflow-under-float
> PROOF was still live). The table resolves alias chains, `from`-imports (incl. `as`), and qualified `m.F`,
> in the module that WROTE the type (a struct field resolves in its DECLARING module). The checker's `let`
> element hint now derives from the resolved `Ty` (`float_elem_hint_ty`), so both sides see the same thing.
> (3) **A variadic `float` param no longer faults**: `fn f(...zs: float)` + `f(1, 2.5)` checked clean and
> trapped at runtime ("expected number, found List") — the prologue coerced the slot holding the PACKED
> list; it now skips `is_variadic` (the elements are coerced by the list peephole). (4) **`Any`/protocol
> element contexts now AGREE**: the compiler's peephole is type-blind and fires inside `List[Any]` /
> `Map[_, Any]` / the `...xs: Any` pack, where the checker had kept the element `int` — so the CHECKER now
> applies the same element widening BEFORE the expected-type check (`elem_widen`, `checker/pattern.rs`):
> a mixed untyped-numeric-CONSTANT literal has `float` elements in EVERY element context
> (`xs: List[Any] = [1, -2.5]` → `[1.0, -2.5]`, as it already did on `main` for `[1, 2.5]`), and a TYPED
> int element is still never touched. Dropping the old gate's `contains(Float)` requirement also makes the
> documented all-int annotated literal true: `xs: List[float] = [1, 2]` / `m: Map[str, float] = {"a": 1}`
> now ADAPT (they were a spurious error; the compiler's hint already coerced them). (5) The
> `float(x)` note is no longer attached to an untyped int CONSTANT at a non-widening sink (`ch.send(1)`,
> an enum payload) — it said "a typed int never widens" about a value that is not typed. +8 tests
> (4 checker incl. the fn-value/alias/note/all-int-annotated cases, 4 two-engine parity RUN tests incl.
> the alias sinks, the variadic param, the `Any` element agreement + its typed-int guard); RED-first
> (6 of the 8 fail on the pre-fix branch, the other 2 lock in behavior the fix makes principled).
>
> **Follow-up 2 (same branch, 2nd adversarial review): the alias table's own scope holes + one more
> erased sink.** (1) **A generic TYPE PARAM shadows a module float alias.** `FloatAliases::is_float`
> matched a bare `Type::Named` against a flat per-module alias set with no scope awareness, so
> `type F = float` + ANY generic decl whose param is also named `F` (`fn g[F](x: F) -> F`, `struct S[F]`)
> made the BACKEND coerce values whose static type is the type VARIABLE — the mirror bug: a runtime
> `Float` under a static `int` (`g(5)` printed `5.0`, `S[int](MAX)` destroyed precision) and a check-clean
> **runtime fault** on a non-numeric instantiation (`g("hi")` → "expected number, found str"). The compiler
> now carries the generic params in scope (`Compiler::float_shadow`, plus `struct_generics` for the ctor's
> field types, which are written in the STRUCT's scope) and excludes them at every coercion site — the
> checker's `resolve_type` already scoped them this way. (2) **A whole-collection ALIAS is not an element
> hint.** The checker licensed `xs: LF = [1, 2]` (`type LF = List[float]`) off the RESOLVED `Ty::List(Float)`
> while the backend's `elem_hint` matches the SYNTACTIC `List[…]`/`Map[…]` shape only → no `Op::CoerceFloat`,
> an `Int` under a static `float` (a NEW leak this branch had introduced with the all-int annotated literal).
> The checker now gates its hint on the same syntactic shape (an aliased ELEMENT — `List[F]` — still widens).
> (3) **A generic-ERASED method param no longer widens.** A method declared `fn set(self, x: T)` on a
> `Box[float]` substitutes `T→float` in the checker, but `emit_float_param_prologue` keys on the DECLARED
> `T` and emits nothing — so `b.set(1)` landed an `Int` in a `float` field (both PROOF programs reproduced:
> integer overflow under `float`, unsorted `List[float]`). The arg check for a SUBSTITUTED param list
> (`check_args_subst`, struct/enum/newtype methods) now keys the widen license on the PRE-substitution
> declared type; a fn-typed struct FIELD is strict too (it is a fn value). A param DECLARED `float` on a
> generic struct still adapts. **Known limit (pinned by test):** a variadic `float` param adapts an untyped
> int constant only with an untyped float constant sibling (`f(1, 2.5)` ✓, `f(1, 2)` ✗) — the packed
> `List[float]` has no coercion site; upgrade path is a list-aware `Op::CoerceFloat` at the variadic slot.
> +4 tests (2 checker rejections, 2 two-engine parity RUN incl. an over-rejection guard), all RED first.

> **✅ FEATURE — multi-line pipe chains + `iter.sum` (2026-07-13, `auto-task/pipe-multiline`).** A line whose
> FIRST token is `|>` now **continues the previous logical line**: the lexer suppresses that line's
> Newline/Indent/Dedent (same escape hatch as the existing `bracket_depth` suppression — new non-consuming
> lookahead `pipe_continues_next_line`, `src/lexer/mod.rs` STEP D), so an un-parenthesized chain
> (`xs\n    |> f()\n    |> g()`) lexes to the exact token stream of the one-line form — parser/VM see nothing
> new, parity is by construction. Blank + comment-only lines inside a chain are skipped; only the exact `|>`
> continues (`|`, `||`, `|=` do not); a **trailing** `|>` stays a parse error (out of scope). Layout-safety
> fences: indent-stack integrity (multi-line chain nested in `fn`/`if` == one-line token stream), true spans
> after a suppressed region, 10k-blank-lines + file-ending-in-`|>` forward-progress tripwires.
> `std/iter.chz` gained `sum(xs: List[int]) -> int` (delegates to the native `xs.sum()` method; empty → `0`,
> mirroring it) so a pipe's right side — which must be a free **call**, never a method — can end in a sum.
> **int-only** and deliberately so: the native method is gated on a numeric element type, so a generic
> `sum[T](xs: List[T]) -> T where T: Add` does not type-check (`sum() requires a numeric list, found List[T]`)
> and a pure-Chezzi generic has no typed zero for the empty case; relaxing the checker gate would re-open the
> MONOID hole 02586a0 closed. A generic/float `iter.sum` needs that gate re-litigated — its own milestone.
> Docs: `docs/syntax.md` §11 (now a real, running example + the continuation rule), `docs/spec.md` tour,
> `docs/stdlib.md` (`std.iter`), `docs/grammar.bnf` (layout-continuation note; token classes unchanged →
> no production change), `tests/corpus/accept/pipe_multiline.chz` (conformance).

> **📝 DOCS — pre-freeze Low clarifications (2026-07-12, `docs/low-clarifications`).** Documented four
> known-and-intended behaviors surfaced by the pre-JIT hunt (no code change): (1) protocols are
> module-local — not qualified-reachable, bare-importable, or usable as a cross-module bound (a possible
> future milestone, not a bug); (2) `MIN % -1` faults like `MIN / -1` (i64 checked-op overflow, not
> Python's `0`); (3) a `panic` inside a `defer` during unwind replaces the in-flight panic (last wins,
> original dropped); (4) `math.round` is half-away-from-zero while the `:.0f` format spec is banker's.

> **✅ BUGFIX — qualified generic turbofish in expression position (2026-07-12, `auto-task/qualified-turbofish`).**
> `mod.Type[int].Variant(...)` / `mod.Type[int].staticmethod(...)` on a whole-module-imported generic
> type was wrongly rejected with a *lying* diagnostic (`module 'mod' has no member 'Type'`, though the
> type plainly exists). Root cause: downstream variant/static resolution is fully key-driven and already
> worked for imported types, but the turbofish-HEAD recognizer (`type_apply_head`) + its key computation
> were **bare-only** (`struct_names`/`enum_names`/`bare_key`), which whole-module imports don't populate.
> Fix: `type_apply_head` now returns the resolved runtime key and recognizes a qualified single-arg head
> (`mod.Type[int]` via `imported_modules` + `module_sigs.{struct_defs,enum_defs}` + `type_key`); the
> compiler gained a matching `qualified_turbofish_key` and additive `NewEnum`/`CallStatic` lowering blocks
> (incl. the combined `mod.Box[int].make[str](x)`). Single-arg only — *multi-arg* qualified
> (`mod.Pair[int,str].X`) stays deferred (no qualified parser carrier; clean parse-error, no panic).
> 6 tests (checker `entry_ok`/`files_reject` + `src/vm/parity_tests.rs` run-both). Docs: `docs/syntax.md` §7a.
>
> **✅ BUGFIX — `parallel:` block defer/join order (2026-07-12, `auto-task/parallel-defer-join-order`).**
> A `defer` directly inside an explicit `parallel:` block flushed *before* the block's spawned children
> joined (violating the documented "defers run after the join" invariant that already held for the
> implicit function-body nursery). Pure compiler emission-order fix in `compile_parallel`
> (`src/compiler/mod.rs`): the closing `LeaveDeferScope` now lands *after* `JoinNursery`
> (EnterNursery, [EnterDeferScope], body, JoinNursery, [LeaveDeferScope]). Defer-free blocks stay
> byte-identical; the break/continue jump-out drain path is unchanged (each marker still pops once). 3
> parity tests in `src/vm/parity_tests.rs` (ordering, deferred-`ch.close()`-after-join, break-once).
> Docs: `docs/concurrency.md` join-semantics bullet.
>
> **✅ BUGFIX — `std.json` RFC-8259 conformance (2026-07-12, `auto-task/json-escape-leading-zero`).**
> Three same-file fixes in `std/json.chz`: (1) `stringify` now `\u00XX`-escapes control chars
> `U+0000..U+001F` (Go `encoding/json` policy) instead of emitting raw bytes → always-valid JSON;
> (2) `parse` rejects leading-zero integers (`01`/`007`/`-01` → `Err("invalid number: leading zero")`,
> matching Python `json.loads`; `0`/`-0`/`0.5`/`0e1` stay valid); (3) `parse` rejects raw control chars
> inside a string literal. Pure-Chezzi, byte-identical serial/M:N parity. 3 parity tests in
> `src/vm/parity_tests.rs`.

> **✅ BUGFIX — `tuple` reserved at declaration (2026-07-12, `fix/tuple-reserved-name`).**
> `is_reserved_type` (`src/checker/mod.rs`) omitted `tuple`, so `struct`/`enum`/`newtype`/`type`/
> `protocol tuple` all slipped the reserved-name gate that rejects every sibling global (`int`, `List`,
> `range`, …). Low severity (`tuple` carries no reachable bare-type/ctor, so it shadowed nothing) but a
> one-way-ratchet consistency gap. Added `|| name == "tuple"`; all five decl forms now reject
> `type 'tuple' is reserved (builtin)`, tuple literals unaffected. Test `user_decl_named_tuple_rejected`.

---

## Current focus

**Live phase (2026-07-23):** pre-JIT/pre-freeze **bug-hunt + drift-fix hunt** — Go-concurrency,
checker↔runtime, and IO drift; live ledger in `docs/gaps.md`. **M19 — Perf track** is paused
in-progress alongside it (see "Next perf batch" below). The Type-Conversion track below is a
**completed/paused** milestone, kept for reference, not the current focus.

### Paused track — Type Conversion (2026-07-07)

**🎯 TYPE CONVERSION — `Convert`/`From` PROTOCOL + SCALAR FILLS.** **STATUS (2026-07-07): Phase 0 ✅ +
Phase 1 slices 1–2 ✅ SHIPPED; slice 3 (`T.convert`) ⛔ deferred (Option A clear-error landed); Phase 2
(multi-source) ⛔ deferred — both revisit-later, see below.** What works now: scalar fills (`bool(x)`,
`parse_int`/`parse_float`); a reserved bound-only `Convert[S]` protocol with SOUND structural witnessing;
direct `Type.convert(x)` conversion (struct↔struct/enum/newtype). What's deferred: generic construction
`T.convert` through a bound (needs witness-passing) and multi-source (needs overloading). This is a
coherent stopping point — the Convert track is PAUSED here pending real demand for the deferred pieces.

Closes the documented gap in `docs/spec.md` "Type conversions & casting" / `docs/future.md §15`: today
conversion is a fixed builtin set (`int`/`float`/`str`/`ord`/`chr`, safe `to_int`/`to_float`) + one-way
`int→float` widening + one-way newtype wrap/unwrap — **no extensible mechanism**. Brainstorm outcome:
build a structural `Convert[S]` protocol, **phased**, anchored on a reference generics model so it stays
consistent (this lang's generics are the buggy-if-improvised part — anchor or bust).

- **Reference model = Java (erased) generics.** Chezzi generics are **erased** (`docs/future.md §14`
  erasure contract), so Java — the only mainstream *erased* model — is the anchor, NOT Rust/Swift (their
  `T.from`-anywhere freedom comes from monomorphization / reified witness tables, machinery that
  contradicts erasure; copying their ergonomics without their mechanism is exactly how generics go
  inconsistent). Transfers directly: dependent bounds allowed, bounds checked statically at BOTH
  definition-site (body may *use* the bound) and call-site (must *prove* concrete satisfaction), and the
  "can't construct through an abstract typevar" wall is Java's wall too.
- **Determinacy rule (the key safeguard).** Every type param must be determined — appear in a value-param
  position (inferable bottom-up) OR be explicit turbofish. A param appearing ONLY inside another param's
  bound (e.g. `T` in `fn[T: Comparable, U: From[T]](x: U)`) is a **static error** ("type parameter `T`
  not determined by any argument — annotate it"), NOT a crash. This is what makes pathological/creative
  bound signatures fail loudly at the checker instead of at runtime.
- **Bound checking rides existing `satisfies_args`** — `From` adds ZERO new inference power; it's another
  predicate like `Comparable`. Call-site algo unchanged: infer params from arg types → substitute →
  verify each bound in dependency order (resolve `T`, then check `U: From[T]`; topological over the bound
  graph) → any `Unknown` defers (existing behavior). Contained blast radius.
- **Phase 0 — scalar fills — ✅ LANDED (2026-07-06, `auto-task/scalar-fills`).** Two additive fills,
  each mirroring an existing pattern exactly (no From-protocol/generics work): (1) `bool(x)` global
  scalar-conversion ctor — a sixth `native ctor` beside int/float/str/bytes/bytearray, wired through
  the identical touch points (prelude decl → RESERVED_CALLABLE → PRELUDE Ctor row → checker
  `infer_named_call` arm mirroring `str` → VM `do_call_builtin` + `builtin_bool`). Truthiness: int
  `0`→false; float `0.0`/`-0.0`→false, NaN→true (Rust `f!=0.0`, Python parity); bool identity; str
  ``""``→false else true (non-empty, NOT a parse — `bool(" ")` is true); scalar-newtype unwraps;
  never faults on a scalar (non-scalar faults like int()/float()). (2) `s.parse_int() -> Result[int,
  str]` / `s.parse_float() -> Result[float, str]` — Result-returning, error-message-carrying siblings
  of the Option-returning `to_int`/`to_float`, wired the same way (STR_METHODS + str_method_sig +
  vm/call.rs arms, `alloc_enum("Result", …)`). Two-engine goldens (serial==M:N) for all three +
  checker type tests + `examples/str_methods.chz` golden extended. Docs: spec.md/stdlib.md/syntax.md/
  future.md §15 updated ("No bool(x)" / "No Result-returning parse" lines removed).
- **NAMING (decided 2026-07-06): protocol `Convert[S]`, method `convert` — NOT `from`.** `from` is a
  **hard keyword** (`src/lexer/mod.rs:216`, the `from X import Y` import syntax), so `fn from(...)` does
  not even parse (`expected identifier, found 'from'`). Rather than make `from` a contextual keyword
  (parser risk), use the non-reserved `convert` — which pairs EXACTLY with the protocol name (Rust
  `From`/`from` coherence, here `Convert`/`convert`), is self-documenting, and matches the existing
  "Convert/From protocol" naming in `docs/future.md §15`/`docs/spec.md`. The method is **target-keyed**
  (`Target.convert(source)`), so a `to`/`into`-family name would read backwards — `convert`/`of`/`make`
  are the right family; `convert` wins on protocol-name match. This makes slice 1a a trivial reserved
  name, NOT a parser change.
- **Phase 1 — `Convert[S]`, single-source, INFALLIBLE.** Parameterized protocol (reuses the just-landed
  first-class parameterized-protocol machinery above), reserved + file-backed in `prelude.chz` like
  `Comparable`. Method `convert(x: S) -> Self` (static — first param not `self`), **return `-> Self`
  only** (design A, decided 2026-07-06). Call shape `Target.convert(x)` — explicit static call,
  conversion always visible in source; **no `Into`, no implicit coercion sites** (fits bottom-up
  inference). Structural witnessing via extended `satisfies_args` (static slot). ONE `convert` per type
  this phase. Covers struct↔struct, enum↔enum, newtype, and the generic bound `[T: Convert[str]]`.
- **FALLIBILITY: infallible protocol; fallible lives OUTSIDE it (corrects the earlier "just return
  Result, no TryFrom" note).** The earlier note was WRONG for the generic case: return shape only
  matters at the **generic bound** — `[T: Convert[str]]` calling `T.convert(s)` must know statically
  whether it yields `T` or `Result[T,E]`; if witnesses could return either, the bound is ambiguous and
  the whole generic payoff breaks. So the protocol **commits to `-> Self`**. Fallible conversions are
  ordinary **Result-returning named static ctors** (already work today, e.g. `Email.parse(s) -> Result[Email,str]`
  in `docs/syntax.md §7a`) — they just don't ride the `Convert` protocol/generic (generic-over-fallible
  is rare). A `TryConvert[S]: try_convert(x) -> Result[Self,E]` protocol is a **clean additive follow-up**
  (exactly how Rust shipped `TryFrom` after `From`) — built ONLY if generic-over-fallible proves needed.
  Design C (Result-only protocol) is REJECTED: forces `Ok(...)` boilerplate on conversions that never fail.
- **`T.convert(x)` construction-through-typevar — ⛔ SLICE 3 DEFERRED (2026-07-07); spike proved the
  "restricted" model delivers NOTHING here.** The Q4-C plan assumed `T.convert` would appear at
  *concrete-pinned* sites the checker could rewrite. A spike disproved it: (a) `T.convert` inside a
  generic body sees `T` **abstract** (`unknown name 'T'`); (b) the SAME gap hits every generic static
  method — existing `T.empty()` fails identically, it's not Convert-specific; (c) concrete turbofish
  `Box[int].empty()` **works**; (d) there is **NO monomorphization** — generic bodies are checked once,
  abstractly (grep-confirmed, `src/checker/proto.rs`). So `T` (spelled `T`) is NEVER concrete while its
  body is checked; the only concrete static call is `Type.convert(x)` written directly — which already
  works today with no protocol. Restricted `T.convert` therefore has no valid call site.
  **Option A landed instead (2026-07-07):** `T.<static>()` on an in-scope type param now gives a CLEAR
  actionable error ("cannot call a static method through the generic type parameter 'T' … call the
  concrete type's static method directly or pass a `fn(...) -> T`") instead of cryptic `unknown name 'T'`
  (`src/checker/expr.rs`, mirrors the newtype-member arm; test `static_call_through_type_param_is_clear_error`).
  **REVISIT LATER** via the deferred **witness-passing** escape hatch below (the only erasure-compatible
  way to make `T.convert` on an abstract `T` real) — build it only when real code needs generic-over-Convert
  construction; distinct-named static ctors + direct `Type.convert` cover the common case until then.
- **BOUND-ONLY vs value-position — ✅ RESOLVED (slice 2, 2026-07-07).** The spike showed a static-ctor-
  only protocol "matched" a VALUE annotation (`fn takes(c: Convert[int])` accepted `Port(1)` with no
  error) — unsound, since a value can't call a static ctor. **Decided + enforced bound-only:** ANY
  protocol with ≥1 static method requirement (`Convert[S]` and future/user static-ctor protocols) is
  now usable ONLY as a generic bound `[T: Convert[S]]` and REJECTED in every value-annotation position
  (keyed on the structural static-slot property, gated at `resolve_type`; instance-method parameterized
  protocols stay value-usable). See the slice-2 landed note below.
- **Phase 2 — multi-source — ⛔ DEFERRED / REVISIT LATER (2026-07-07); needs OVERLOADING, thin payoff.**
  A type witnessing `Convert[Celsius]` AND `Convert[Kelvin]` = two `convert` methods on one type — but
  Chezzi has **no overloading** (`method 'convert' is already defined`; hit in a spike). So Phase 2
  REQUIRES introducing argument-type overload resolution (the "type-argument-keyed witness selection"
  framing: resolve `Type.convert(x)` by `typeof x` → the `Convert[typeof x]` witness, tie-break **exact
  source beats `int→float` widening**). Coherent (Rust's model, not ad-hoc), but it BREAKS the no-overload
  invariant, and the payoff is thin: it only helps **direct** multi-source calls, and distinct-named
  static ctors (`Fahrenheit.from_celsius`/`from_kelvin`) already do that today with zero new machinery.
  It does NOT unblock the generic payoff (that's slice 3 / witness-passing). **Decision (2026-07-07): not
  worth the overloading cost now** — revisit together with witness-passing when real code demands
  multi-source; until then use distinct-named static ctors. General method overloading stays banned.
- **Witness-passing (Haskell/Swift dictionaries) — DEFERRED escape hatch.** The only erasure-compatible
  way to make `T.convert` work on an *abstract* `T` (thread the concrete `convert` in as a hidden arg).
  Its own scoped milestone, built ONLY if the restricted-construction DX proves too weak. Do NOT bolt on
  per-call-shape special cases instead — that's the path to inconsistent generics.
- **NOTHING to reserve for `convert` — it's a plain method name.** Only the **protocol `Convert`** is
  reserved (prebuilt + file-backed in `prelude.chz`, like `Comparable`/`Add`/`Stringable`; user
  redeclaration rejected "reserved (builtin)"). NOTE: a user can ALREADY write `fn convert(c: Celsius)
  -> Fahrenheit` + call `Fahrenheit.convert(c)` TODAY via plain static-method dispatch — no protocol
  needed for DIRECT conversion. The protocol adds ONLY: (a) the generic bound `[T: Convert[S]]`, (b) a
  standard reserved conversion interface. So Phase 1's real substance is witnessing + `T.convert`, not
  direct-call plumbing.
- **SLICES (risk-ranked):** 1 declare `Convert[S]` prebuilt protocol in prelude + reserve the name
  (LOW, mechanical — mirror `Comparable`) — **✅ LANDED (2026-07-06, slice 1)**: the parameterized
  STATIC protocol `Convert[S]` (`fn convert(x: S) -> Self`, `is_static`) is file-backed in `prelude.chz`,
  seeded in `prebuilt_protocols()` (drift-guarded), reserved via `is_reserved_protocol` (user redeclare +
  `struct`/`enum`/`newtype`/`type Convert` rejected `reserved (builtin)`), and binds as a generic bound
  `[T: Convert[int]]` with arity-checking (`[T: Convert]`/`[T: Convert[int,str]]` → clear arity error).
  Checker-only, runtime-erased (no `Ty::Protocol` reaches the VM). NOT wired: witnessing (a concrete type
  satisfying it — slice 2) or `T.convert(..)` through a bound (slice 3, still errors `unknown name 'T'`).
  · 2 **sound static-slot witnessing + bound-only enforcement**
  (HIGH — checker soundness) — **✅ LANDED (2026-07-07, slice 2)**: (A) structural conformance is now
  `is_static`-aware — a concrete type witnesses `Convert[int]` (satisfies `[T: Convert[int]]` at a call
  site) IFF it has a **STATIC** `convert(x: int) -> Self`; an instance/`self`-slot `convert` (even at
  matching arity, `convert(self) -> Self`), a wrong-source (`convert(x: str)`), a wrong-return
  (`convert(x: int) -> Other`), or a missing `convert` all correctly reject. Wired in `method_matches`
  (`proto.is_static == actual.is_static`) + `satisfies_methods` (carries the requirement's `is_static`);
  `hoist_protocol` now computes a protocol method's `is_static` from its first-param name so USER
  static-ctor protocols are covered too. `Unknown` still defers. (B) bound-only: a protocol with ≥1
  **static** method requirement is now REJECTED in every value-annotation position (param/field/return/
  binding/reassign slot AND nested — `List[Convert[int]]`, `Option[Convert[int]]`, tuple element, `type`
  alias body) with `protocol 'Convert' has a static method and can only be used as a bound, not a value
  type`. Keyed on the STRUCTURAL static-slot property (not the name): `protocol_has_static_method` scans
  a protocol's OWN methods AND its **transitively-embedded** requirements (so a bundle
  `protocol MakeInt: Convert[int]` that EMBEDS a static-ctor protocol is bound-only too), and a
  **cross-module** alias body (`import Foo from a` / qualified `a.Foo`) — resolved by the read-only
  resolver (no gate) and returned pre-resolved — is re-gated at the two consumer seams by a recursive
  `Ty`-walk (`first_static_ctor_protocol`). The general instance-method parameterized-protocol value
  position (`c: Container[int]`) is UNAFFECTED (regression-guarded). **This CLOSES the value-position
  false-accept** (the spike where `fn takes(c: Convert[int])` accepted `Port(1)` with no error — the
  open item below — plus the embed + cross-module-alias laundering vectors). Checker-only,
  runtime-erased; positive witness runs byte-identical serial == M:N.
  · 3 **`T.convert`
  static-through-bound, concrete-pinned checker rewrite** (HIGH — generics, hand-do + runtime-verify) —
  **PENDING** (`T.convert` through a bound still errors `unknown name 'T'`). Memory: auto-task
  over-reaches on checker soundness → auto-task slice 1 only, hand-do 2/3.
- **Out of scope:** `Into`/`x.into()` (needs top-down expected-type threading; Chezzi is bottom-up),
  `cast[T](Any)` (needs runtime type tags — separate reflection milestone, `docs/future.md §14`),
  `TryConvert` (deferred additive), general method overloading (this is type-keyed witness selection).

**✅ MODULE-ROOT "ONE ROOT PER RUN" — silent-wrong-module fix + spec correction (2026-07-11).** A bare
`chezzi run` derived the module-graph root **twice** from different origins: `main::resolve_entrypoint`
found the manifest by walking up from the **cwd** (→ located the entry file), then both graph builders
(`main::type_check` + `vm::run_file_inner`) re-derived a *second* root by walking up from the **entry
file** (`resolver::find_root`), which stops at a **nested** `chezzi.toml`. When both roots held a
same-named module, imports silently resolved against the inner one — **wrong module, exit 0, no
diagnostic** (repro: outer `shared.chz` + nested `services/chezzi.toml` + `services/shared.chz`; bare
run printed `INNER shared` instead of `OUTER shared`). FIX: thread the already-computed manifest root
into BOTH builders so the root is computed **exactly once per run** and reused for entry-location AND
every import. `resolver::build_graph_with_root(entry, root)` (routes through the same
`build_graph_impl`/`Builder`, so cycle-detection + `MAX_IMPORT_DEPTH=256` are unchanged);
`vm::run_file_with_entry`/`run_file_engine`/`run_file_inner` gained a `root: Option<PathBuf>`;
`main::resolve_entrypoint` now returns the root and `run()` pins it into `type_check` **and**
`run_file_with_entry` (both — else checker/VM disagree). Explicit `chezzi run <file>` is unchanged
(`root=None` → walk up from the file = nearest marker, the correct Go/Cargo/npm sub-package behavior).
Both engines identical (serial + M:N route through `run_file_inner`). Tests: new `tests/module_root.rs`
(CLI-level, real on-disk trees — the bug is invisible to the library `build_graph` helpers): bare run
(both engines) → OUTER, explicit file-run → INNER, single-root bare/file/cwd-invariance agree. SPEC:
`docs/spec.md` "One root governs the whole graph" rewrote the false "nested `chezzi.toml` is silently
ignored" claim to the actual **nearest-marker-from-origin** rule (origin = entry file for `run FILE`,
cwd/manifest for bare run) + the fixed-once invariant.

**✅ PARAMETERIZED PROTOCOLS IN VALUE/ANNOTATION POSITION (2026-07-06).** A parameterized protocol is
now a first-class **value/annotation type** — `c: Container[int]` is valid as a param, return, struct
field, and reassignment slot (was a hard "parameterized protocol N can only be used as a bound, not as
a value type" error). Ships DECISION-1 option (a): carry the concrete args on the checker-only
`Ty::Protocol(String, Vec<Ty>)` variant, **witness conformance statically at every store/pass
boundary** (`assignable` → `satisfies_args`, shared by all four write-sites), then **erase at runtime**
(no `Ty::Protocol` exists in vm/compiler — grep-verified — so the variant change is contained to
`src/checker/**` and cannot diverge the two engines). **Method-return element RECOVERY** is on: at
`c.get(0)` where `c: Container[int]`, the protocol's own type-params are substituted → the carried args
into the method's params AND return, so it yields `int`, not the bare `T` (sound — the store/pass
boundary already witnessed conformance; same model as `Iterator.next() -> Option[T]`). **STRICT
INVARIANCE**: `Container[int]` ≠ `Container[str]` ≠ bare `Container` (exact-arg match via the existing
conservative `compatible`, no value-position subsumption). `Iterator[T]` stays on its
`Ty::Struct("Iterator",[T])` path (not merged). Touch points (all `src/checker/**`, additive):
variant `ty.rs` + all hand-written destructures fixed via the non-exhaustive-match exhaustiveness
guarantee (compatible/Display/error_proto/assignable/satisfies-subject/sendable_rec/fill_ret/subst/
bare-construction arms), the rejection→accept flip (`sig.rs`, arity-checked against `pinfo.type_params`),
the read-only resolver Generic arm (`proto.rs` — mints `Ty::Protocol` so it no longer silent-accepts as
`Unknown`), and the method-return recovery arm (`expr.rs infer_method_call`). Cross-module qualified
protocols are not exported as value types (rejected at the annotation, not silent-accepted). Tests:
11 checker units (accept/wrong-arg-reject/bare-non-generic/both-direction-invariance/return recovery/
three write-site rejects/nesting) + 3 two-engine goldens (`vm::tests`, byte-identical serial-VM == M:N).
Docs: `docs/syntax.md` (limitation lifted), `docs/spec.md` (M14 rows).

**✅ MUTABLE-CONTAINER GENERIC INVARIANCE — covariance soundness hole closed (2026-07-07).**
(HIGH — checker soundness.) The M14 "strictly invariant" guarantee (parameterized protocols) now also
holds for the built-in MUTABLE containers + user generic structs/enums. Before: `Checker::assignable`
compared List/Set/Map/Struct/Enum type ARGUMENTS covariantly (`self.assignable(e, a)`, protocol-aware),
so `assignable(List[Any], List[Cat])` reduced to `assignable(Any, Cat)`=true — a `List[Cat]` VARIABLE
flowed into a `List[Any]` slot, and the callee could `.push` a non-Cat back through the shared alias
(check-ok → runtime `cannot read field 'x' of int`, or a SILENT wrong answer). The reverse direction was
already correctly rejected — a one-directional covariance bug. Fix (checker-only, `src/checker/proto.rs`
~374): the MUTABLE, by-reference containers `List`/`Set`/`Map` and user generic `Struct`/`Enum` now
compare type args with the context-free structural-equality primitive `compatible` (= strict
INVARIANCE), mirroring the M14 `compatible` Protocol/Struct/Enum arms + `bound_args_match`. The IMMUTABLE
carriers `Option`/`Result` keep covariant element assignment (no write-through alias → sound; preserves
`?`/Result/Option plumbing). Covers List element, Map value/key, and user generic struct fields (mutated
via a `set(self, x: T)` method), through BOTH the `Any` top type AND a user protocol supertype, at BOTH
the fn-arg and let/`:=`/return boundaries. Literals stay clean: an annotated/expected-directed literal
(`xs: List[Any] = [1, "a", true]`) is built AS `List[Any]` by `infer_list`'s expected-directed path
(pattern.rs:1646), so only container VARIABLES flowing container→container are newly rejected. Iterator[T]
is unaffected (all use flows through generic BOUNDS `[S: Iterator[T], T]` / generator yield-validation,
never a direct `assignable(Iterator[Sub], Iterator[Super])`). Tests: 6 checker units (repro A launder,
Map value, user-generic-struct via Any AND user-protocol, let/`:=` boundary, + a neighbor-preservation
battery), all on the REAL graph path (`entry_ok`/`entry_rejects`). Full checker suite (incl.
`all_shipped_examples_typecheck` + the M14 nesting/invariance tests) green — no over-rejection. Aligns
impl with the already-documented contract (spec.md "strictly invariant", future.md "no covariance holes")
— no spec change. Checker-only; VM/runtime dispatch untouched.

**✅ SCALARS INTRINSICALLY SATISFY `Stringable` (2026-07-06).** `int`/`float`/`bool`/`str` now satisfy
the prebuilt `Stringable` protocol (sole method `str(self) -> str`) intrinsically, so a
`fn show[T: Stringable](v: T)` generic accepts them — closing the last inconsistency where every other
scalar-friendly builtin protocol (`Comparable`/`Hashable`/`Add`/…) already had an intrinsic scalar arm
in `satisfies_args_d` but `Stringable` did not. Coordinated checker + VM change (a checker-only arm
would type-OK then runtime-trap `v.str()` on an int): (1) checker `satisfies_args_d` scalar arm
(`src/checker/proto.rs`, beside the Comparable arm — all FOUR scalars, unlike Comparable/no-Bool or
Hashable/no-Float); (2) VM `do_method_call` scalar `str` branch (`src/vm/call.rs`, before the
`Value::Obj` guard, mirroring the `compare` branch — `stringify`→alloc `Obj::Str`→push; the already-`Str`
receiver for `T=str` is intercepted + re-alloc'd or it would fault in struct dispatch). Bound-only like
every sibling intrinsic arm: a direct `(5).str()` on a concrete scalar stays a compile error (matches
`(5).compare(3)`; use the free `str(5)` builtin). Parity: serial-VM == M:N-VM (both share
`do_method_call`). Tests: `checker::tests::stringable_scalar_satisfies_ok` + `scalar_str_direct_call_is_bound_only`,
`vm::tests::primitive_str_method_on_vm` (two-engine). Docs: `docs/syntax.md`/`docs/spec.md`/`docs/stdlib.md`.

**✅ EXACT-DUPLICATE LITERAL MATCH ARMS ERROR (2026-07-05).** `match n: 1: … 1: …` (and `"x":` twice,
`1 | 1`) is now a `duplicate match arm` error — dead code under first-match — matching the existing
enum-variant dup detection (was silently accepted, a diagnostic inconsistency). Same guard carve-out
(`1 if c:` then `1:` stays legal). Range *subsumption* (a literal inside an earlier covering range) is
still not flagged. `src/checker/pattern.rs` (the `MatchKind::Literal` arm), tests in `src/checker/tests.rs`.

**✅ `recover:` TRAILING STATEMENT-`match`/`if` IS THE BLOCK VALUE (2026-07-05).** A `recover:` block
whose TRAILING statement is a statement-form `match` (or `if`) with value-producing arms/branches was
typed `Result[nil]` and the produced value was SILENTLY DROPPED (`Ok(nil)`) — only a genuine trailing
*expression* (or the `y := match …; y` workaround) yielded `Result[<arm type>]`. **Now** a total tail
`match` (≥1 arm, every arm body ends in a value `Expr`) or `if/else` (has `else`, every branch/else body
ends in a value `Expr`) is treated as the block's value expression: its unified arm/branch type becomes
the `Result[T]` T, and `Ok(v)` wraps the real value. A non-value tail (a `let`, a non-total/`else`-less
construct, a nested-statement tail) stays `Result[nil]` byte-identically; an all-`panic` tail stays
bottom (`Result[Unknown]`, matching direct `recover: panic(…)`). Fix at BOTH stages that `split_last`
the recover block, gated on ONE shared `crate::ast` predicate (`match_tail_is_value`/`if_tail_is_value`/
`block_produces_value`) so checker + compiler can never drift on which tail is a value: checker
`infer_recover` (`src/checker/pattern.rs`) folds arm/branch trailing-expr types (dedicated
`infer_recover_tail_{match,if}`, statement-form persistent-refine, match/if typing elsewhere
untouched); compiler `compile_recover` (`src/compiler/mod.rs`) reuses `compile_match_{lit,general}` +
a value `run_body` / a value analog of `compile_if` so exactly one value converges pre-`Ok`-wrap (the
`DrainHandlerDefers`/`NewEnum Ok`/`PopHandler` tail stays byte-identical → serial == M:N). Defers inside
a value arm/branch run without clobbering the value (drain touches `frame.deferred`, not the stack). The
recover rejection rules (`return`/`break`/`?`-on-`Option`) are separate and untouched.
**Follow-up fix (heterogeneous arms → fall back to nil, not an error):** the syntactic
`match_tail_is_value` predicate is true even when arms produce genuinely *different* types (a void
`print(...)` arm mixed with an `int` arm, or `str` vs `int`) — those have no single value type. The first
cut folded them with the erroring `unify_branch`, which REGRESSED previously-valid fault-isolation
`recover:`s (`Ok(_)` consumer) into `branches have incompatible types`. Now the checker folds with a
NON-erroring `fold_recover_tail`: uniform arms → `Result[T]`; the moment two arms are incompatible it
latches non-uniform and types the block `Result[nil]` (per the design contract "do not force a value
where there isn't one"). The compiler is UNCHANGED — it still compiles the tail as a value, but because
the block is `Result[nil]` the nil-in-value-position ban makes the `Ok(v)` payload unusable in every
value context, so the heterogeneous runtime value can never be observed (no checker/runtime divergence,
no channel needed). TDD: checker `recover_tail_stmt_{match,if}_{value_*,heterogeneous_*}` + parity
`recover_tail_{stmt_match,stmt_if,match_heterogeneous,if_heterogeneous}_*` (RED-first: the pre-fix
branch binary rejected `match cmd: "a": "hello"; _: 42` with `str and int`). Docs: `docs/syntax.md`
recover section.

**✅ MULTI-BRANCH RETURN INFERENCE — JOIN merge + finalize, `Unknown`-leak fix (2026-07-05).**
Checker-only (`src/checker/sig.rs` + a closure hook in `src/checker/pattern.rs`); no runtime/VM/grammar
change, so two-engine parity + conformance hold by construction. **Before:** `infer_returns` was
first-concrete-return-wins with `Unknown`-fill — one branch's shape won and complementary type-arg slots
stayed `Ty::Unknown`, which then LEAKED as a type-check bypass (an `Err`-only fn's `Result[Unknown, str]`
let `x := err()?; y: int = x; z: str = x` all type-check). **Now:** ALL `return` branches (plus an
inline/implicit-trailing expr) are typed and folded with a join `J`: `a==b`; the one `{int,float}→float`
widen (bare scalars only, no recursion into slots); the **same** type-constructor (`Result`/`Option`/
`List`/`Map`/`Set` or same generic struct/enum/newtype) → **merge slot-wise** (`Ok("h")` ⊔ `Err("a")` =
`Result[str, Error]` — the error slot defaults to `Error` when its payload satisfies `Error`; see the
2026-07-05 addendum below); otherwise a **conflict** (`cannot infer return type: conflicting branches (X vs Y)`
— NO common-supertype/protocol/`Any` search, so two distinct structs conflict, a protocol return must be
spelled). A post-fixpoint **finalize** defaults the `Result` **error slot** to the `Error`
protocol when un-pinned or its payload satisfies `Error` (matching `T!`, so `fn ok(): return Ok(5)` is `Result[int, Error]`) and REJECTS any other
residual un-inferable `Unknown` (`fn err(): return Err("x")`, `fn none(): return None`, `fn f(): return
[]` — the return-position analogue of the empty-collection diagnostic; also closes the old
baseless-recursion permissive gap). `Ty::Param` (generic fns / the proto.rs HOF loop-back) is left
untouched. Applies uniformly to free fns, struct/enum methods, AND free closures (`f := fn(): Ok(5)` →
`Result[int, Error]`; `fn(): Err("x")` rejected — gated to `expected.is_none() && !generic_arg_prepass`
so `fn`-typed slots / generic-HOF contexts are excluded). Cascade-safe: a body that already errored
suppresses the finalize diagnostic. 18 new checker tests (repro a–e + must-not-break neighbors +
closure-gated); 4 existing tests updated to the new documented semantics (int-vs-str/int-vs-str method
conflicts now say `conflicting branches`; pure self-/mutual recursion now REJECTED not permissive).
Acceptance demo: `fn res(): if …: return Err("a")` then `return Ok("h")` infers `Result[str, Error]`
(byte-identical serial == M:N). Docs: `docs/syntax.md` "Return type inference", `docs/spec.md` widening
note. Full suite (3065) + conformance + clippy green.

**↳ ADDENDUM (2026-07-05) — inferred `Result` error slot defaults to `Error` when the payload
satisfies `Error`.** Refines the above: an inferred error slot defaults to the built-in `Error`
protocol when it is un-pinned (`Unknown`) OR its `Err`-branch payload **satisfies `Error`**. So
`Ok("h")` ⊔ `Err("a")` infers `Result[str, Error]` (was `Result[str, str]`; `str` satisfies `Error`),
and two *different* `Err` payloads that both satisfy `Error` (`Err(EA())` vs `Err(EB())`) unify to
`Error` instead of conflicting. A concrete payload that does **not** satisfy `Error` (a struct with no
`message`, `int`, …) is **preserved**, not laundered. A deliberate concrete error type is spelled
explicitly (`-> Result[str, str]` / `-> int!DbErr`, resolved by `resolve_type`). Implementation
(`src/checker/sig.rs` + `src/checker/pattern.rs`, checker-only): `join_ret` uses `join_err_slot`
(equal→keep, one `Unknown`→other, two `Error`-satisfying concretes→`Unknown`, else conflict);
`fill_ret` and `default_expr_result_e` default E→`Error` iff `Unknown || assignable(Error, E)`. All
three are now `&self` (need the `assignable(Error, ·)` predicate, same check as recover-`?`).
Rationale: uniform, consistent with `T!` / `Result[T]`; accepted tradeoff — a *custom error type that
satisfies `Error`* is erased to `Error` in inference unless annotated.

**Adversarial-review caught a soundness bug in the first cut** (the "always force E→`Error`
unconditionally" version, commit 29513bd): two independent prosecutors + repro showed that forcing a
concrete NON-`Error` payload to `Error` (a) LAUNDERED it in the if/match-expression path
(`x := if c: foo() else: foo()` where `foo -> Result[int, MyErr]`, `MyErr` has no `message` →
`e.message()` check-passed then faulted at runtime — the expr path has no post-hoc re-check), and (b)
OVER-REJECTED forwarding (`fn wrap(): return foo()` newly failed `Result[int] vs Result[int, MyErr]`).
Chezzi does not enforce `E: Error` on the slot, so this was reachable. Remediated by the
`assignable(Error, E)` gate above; both repros now behave correctly (reject the bad `.message()` at
check time, accept the forward). Runtime values unchanged (checker-only). 5 new checker tests total
(intent + soundness + no-over-reject regression guards). Serial == M:N verified; full suite +
conformance + clippy green.

**✅ GENERIC FN AS A VALUE — scope A + B, erased runtime (2026-07-05).** A generic function
(`fn ident[T](x: T) -> T`) is now a usable **value** once its type params are **pinned**: via an explicit
**turbofish** (`g := ident[int]` ⇒ `fn(int) -> int`, scope B) OR against a **known concrete
`fn(...) -> ...`** — an annotation (`h: fn(int) -> int = ident`), a HOF parameter (`applyit(ident, 5)`), a
return position (`fn getf() -> fn(int) -> int: return ident`), or an assignment target (scope A). Two
independent seams, both **same-module** gated so checker-accept ⟺ compiler-erase stay in lockstep:
**checker** (`src/checker/pattern.rs`) — `infer_ident` A-path (unify the declared sig against the
`expected_hint` `Ty::Func`, enforce bounds, return the substituted concrete fn — never `expected`, so an
unsatisfiable target is caught by the existing assignability check) and `infer_index` B-path (turbofish
`ident[int]` → `seed_targs` arity-check + `enforce_bounds` + `subst`); **compiler**
(`src/compiler/mod.rs`) — a tiny Index-arm erase (drop the type index for a non-shadowed top-level fn
name, load the plain fn value). Runtime is generic-**ERASED** (the value IS the underlying function), so
**serial == M:N** is automatic — but every accepted case has a both-engines RUN test (the bind-import
trap). Soundness rejects (all TDD'd, failing-then-green): unsatisfiable pin, bound violation (turbofish +
annotation), turbofish arity mismatch, downstream concrete-type misuse. Must-not-regress: direct calls,
non-generic fn values, call-site turbofish `ident[int](5)`, generic-HOF-param (closure path), and
compiler-erase shadow-safety (a local/param shadowing the fn name is a real index). New checker + parity
tests in `src/checker/tests.rs` + `src/vm/parity_tests.rs`. **Known v1 limits (deferred):** (C)
first-class / rank-N polymorphism — a bare un-pinned generic fn value (`g := ident` then `g(5)`), or one
binding used at two different types, stays a clean error (hint: turbofish or a `fn(...) -> ...`
annotation); and an **imported** generic fn used bare as a value stays the un-pinned error (same-module
only — resolves the accept⟺erase lockstep without a span side-table). Docs: `docs/syntax.md` fn-value
section. Full suite + conformance + clippy green.

**✅ GENERIC FN VALUE PIN NOW COMPOSES THROUGH A BUILTIN `.map`/`.fold` SLOT (2026-07-05).** Scope A
fired for a user-defined HOF param (`applyit(ident, 5)`) but NOT for a builtin container method that
carries its OWN result type param (`[1,2,3].map(conv)` / `.fold(0, add)` were WRONGLY rejected with a
leaked `T`: "argument to 'map' has type fn(T) -> str, expected fn(int) -> str"), while `.filter(keep)`
(fully-concrete slot) and the turbofish workaround (`.map(conv[int])`) worked — an inconsistency.
Root cause: a bare generic fn arg is prepass-typed rigid by `infer_generic_arg_tys` (no `expected_hint`),
so its own `[T]` was never pinned from the method's element slot; `infer_generic_method`'s own-`U`
recovery couldn't simultaneously pin the arg fn's `[T]`. Fix (`src/checker/proto.rs`,
`infer_generic_method`): a per-arg re-pin `try_pin_generic_fn_value_arg` INTERLEAVED in the pass-1 loop
(uses the live subst map so `.fold`'s accumulator `U` is bound by `init` FIRST) — for a bare same-module
generic-fn arg, unify its declared sig against the slot substituted-so-far (`fn(int) -> U`) in a FRESH
map, ACCEPT the concrete result ONLY when every arg-fn param binds AND `ty_fully_concrete` (so a
still-free slot or a genuinely un-inferable return-only arg-fn param defers unchanged — the Category-1
leak guard + every clean reject survive), enforcing the arg fn's declared bounds. Additive checker-only,
runtime generic-erased (serial == M:N automatic; both-engine RUN tests). Must-not-regress verified:
unannotated-closure loop-back (`.map(fn(x): x*2)`), `.filter(keep)`, user-HOF `mymap`, turbofish,
two-distinct-pins-no-launder (fresh map per call), un-inferable `produce[V]` stays a leaked-`V` reject
(not degraded to `List[Unknown]`), and a user struct uniquely owning `.map` still resolves to the USER
method. Tests in `src/checker/tests.rs` + `src/vm/tests.rs`; docs: `docs/syntax.md` fn-value section.

**✅ CHECKER LENIENCY — five decl footguns now rejected (2026-07-05).** All checker-only,
reject-earlier in the decl-hoist pass (`src/checker/setup.rs` Struct/Enum/NewType arms) + a
cascade-suppression tweak in the pass-2 body loops (`src/checker/sig.rs`); no runtime/VM change, so
two-engine parity holds by construction. (1) **Duplicate instance method** (struct/enum/newtype) — was
silently last-wins, now `method 'f' is already defined` at its decl-site span. (2) **Duplicate struct
field** — was first-wins with a dead-but-positionally-required ctor slot, now `field 'x' is already
defined`. (3) **Field + method sharing a name** — now `'f' is declared as both a field and a method of
'P'` (mirrors the enum variant/static disjointness rule). (4) **Same-name method's confusing return-type
cascade** — the pass-2 body loops now `continue` past a duplicate-named method (`filter(count>1)`
guard), so the clear dup error is the sole diagnostic instead of the misleading "expected int, found
str". (5) **Newtype static method** (`fn zero()` — no `self`) — was unreachable → cryptic "unknown name
'Meters'", now a clear "static (associated) methods on a newtype are not supported yet (only struct and
enum have them)" at BOTH the decl site and any `Newtype.method()` call site (the feature stays deferred,
not implemented). Reuse: one `report_dup_names(iter<(name,span)>, kind)` helper (`setup.rs`) drives the
method-dup checks in all three arms + the field-dup check. Tests: 7 new negative tests in
`src/checker/tests.rs` (beside `duplicate_variant_within_one_enum_is_reported`). Full suite + conformance
+ clippy green; `all_shipped_examples_typecheck` unaffected (std/examples sweep found zero real clashes).
Docs: `docs/spec.md` M21 row + newtype note de-staled.

**✅ BUGFIX — `for`/`List()`/`Set()` over a NAMED builtin cursor now CONSUMES it in place (2026-07-04).**
A `for x in it:` (or `List(it)`/`Set(it)`) driven by a NAMED `Obj::Iter` cursor from `xs.iter()` used to
snapshot a private copy and never advance the shared cursor, while `.next()` and struct iterators DID
advance in place — so `for` had opposite semantics depending on the iterand kind, contradicting
`docs/syntax.md` ("reusing one exhausted cursor yields nothing on a second pass"). Fixed: added an
`IsCursor` opcode (mirrors `IsGenerator`); `compile_for` now routes a named/converted cursor onto the
lazy `next()` step (advances the shared heap cursor via `call.rs`), and `drain_iterable` consumes a
cursor in place (clone remaining, advance `pos` to end). Now `it := [1,2,3,4].iter(); for … break at 2;
List(it)` yields `[3, 4]`; a second `for` over the same cursor yields nothing; `next()` after a `for`
returns `None`. Invariants kept: non-cursor collections still fresh-snapshot each loop; `xs.iter().iter()`
is one fresh cursor; a fresh temp `for x in xs.iter():` still fully iterates; generators unchanged.
Serial==M:N byte-identical. (Multi-var `for a,b in named_cursor:` still snapshots — out of scope,
behavior unchanged; noted as a follow-up.) Tests: 6 new in `vm/parity_tests.rs` + golden
`examples/iterable.chz`. Docs: `docs/syntax.md` clause added.

**✅ REFACTOR — split the mega-files + REMOVED the tree-walk interpreter (2026-07-04).** Two parts:
- **File split (behavior-preserving).** `impl Vm` (one ~12.4k-line block) split across
  `vm/{exec,arith,call,sched,netio,stmt}.rs`; `impl Checker` split across
  `checker/{setup,sig,pattern,expr,proto}.rs`; the big inline `mod tests`/`gc_tests`/`parity_tests`
  moved to `vm/{tests,gc_tests,parity_tests}.rs`. `vm/mod.rs` 32,988→~3.5k, `checker/mod.rs`
  17,698→~4.5k. Mechanism: an inherent method's privacy keys off the *impl's module*, so split-out
  private methods are widened to `pub(super)` (visible throughout the parent module, still
  crate-internal). No logic changes.
- **Interpreter REMOVED.** `src/interp/` deleted; the tree-walk engine is gone. Two-engine parity is
  now **serial-VM (`parallel=false`) == M:N-VM (`parallel=true`)** — both are the same `Vm`, only the
  scheduler differs. The ~250 parity tests swapped their oracle interp→M:N (`run_capture`→
  `run_capture_parallel`, added `run_program_parallel`/`run_file_p` helpers). Tests that pinned
  *cooperative-only* semantics (Executor drain order, by-reference capture, racing spawns, GC-stress)
  dropped their now-invalid M:N cross-arm and keep their concrete cooperative expecteds; interp-only
  tests (`interp_rejects_generators`, `bench_vm_faster_than_interp`, …) were deleted. All `interp::`
  refs in prod code were only `/// Mirrors interp::X` doc-comments (no code coupling); `--serial`
  already routed to the VM. Docs updated: `CLAUDE.md`, `main.rs`, this file.

**✅ LANGUAGE — the `pass` keyword: no-op statement + empty protocol/struct bodies, and `Any` wired
into the prelude (2026-07-04).** `pass` is now a REAL reserved keyword (`Token::Pass` in the lexer
KEYWORDS table — reserved-as-a-name BY CONSTRUCTION, `expect_ident` rejects it like `ref`/`fn`), NOT
the discarded parser hack (which string-matched the identifier "pass" only in protocol bodies and
collided with `pass`-as-variable + `protocol pass:`). Three roles off the single token:
- **No-op statement (`StmtKind::Pass`):** modeled on `Break`/`Continue` — parse arm, checker no-op arm,
  compiler emits NOTHING, interp returns `Flow::Normal`, desugar/editor no-op arms. Valid in every
  statement-block position (fn/method body, if/elif/else, for/while, statement-match arm, concurrency
  blocks). A lone-`pass` fn body == a lone-`return` body (falls off end → nil). Statement-only (not
  valid in a closure / expression-match arm — those are single-expression positions; a no-op closure
  is `fn(): nil`). Two-engine byte-identical (compiles to no bytecode).
- **Empty protocol body:** `protocol Foo:` + a SOLE `pass` line = zero methods/embeds → an accept-all
  TOP type (structural ⇒ every type satisfies it). REUSES the existing empty-protocol short-circuit in
  `satisfies_args_d`; NO satisfaction change. A user empty protocol behaves byte-identically to `Any`
  (the accept-all is not keyed on the name — generalization guard test asserts Foo == Any behavior).
- **Empty struct body:** `struct S:` + a SOLE `pass` line = zero fields; `S()` ctor takes no args,
  prints `S()`, structural-equals another `S()`, and is intrinsically `Hashable` (usable as Set/Map
  key). New: a checker `satisfies_args_d` zero-field-struct `Hashable` intrinsic + a VM+interp
  `struct_hash` constant-0 path for a zero-field struct with no `hash` method (parity; `==`'s type-tag
  guard keeps distinct empty-struct types unequal despite the shared hash).
- `pass` is the SOLE-line marker only: `pass`+member and `pass pass` are parse errors (modeled as the
  body being exactly `pass NEWLINE DEDENT`, in both the hand parser and grammar.bnf `<structBody>`/
  `<protoBody>`). Empty ENUM is OUT — `pass` in an enum body is a clear parse error.
- **`Any` wired into the prelude:** `protocol Any:` + `pass` added to `std/prelude.chz` (took the mirrored
  count to 17, was 16; later 18 with `Convert`) + `Any` added to the `assert_native_protocol_shape_matches` drift
  list. `prebuilt_protocols()` stays the Rust source of truth; the prelude is the additive
  drift-guarded mirror. A USER redeclare of `Any` stays rejected (`is_reserved_protocol`); the prelude
  is exempt via the validate-and-no-op stdlib hoist. Empty protocols are NO LONGER "unparseable".
- Docs (same commit): docs/syntax.md `pass` section + Any update; docs/spec.md `pass` keyword + empty
  structs note; docs/grammar.bnf `PASS` terminal + `<passStmt>`/`<structBody>`/`<protoBody>`; corpus
  accept/reject files; 3 two-engine goldens (`examples/pass_noop`/`empty_protocol`/`empty_struct`);
  regenerated the editor TextMate grammar (pass now highlights as a keyword).

**✅ LANGUAGE — variadic parameters (`...xs: T`) + the `Any` top type, and `print` ported off its last
synthetic signature (2026-07-03).** One coherent feature in two phases.
- **`Any` top type:** an EMPTY structural protocol (zero methods) so EVERY type satisfies it — scalars
  included. Seeded in `prebuilt_protocols()` + added to `is_reserved_protocol` (now ALSO prelude-mirrored
  + drift-guarded — the `pass` keyword made empty protocol bodies expressible; see the entry above). The one real fix: `satisfies_args_d` now short-circuits `Ok` for any
  zero-embed/zero-method protocol right after the `Ty::Unknown` guard, so an empty protocol is a genuine
  top type for *every* `Ty` (before, only structs passed it — scalars fell through to `_ => Err`). NOTE:
  this generalizes to ANY user-declared empty protocol (correct semantics of an empty structural
  interface, additive). Not dynamic typing — an `Any` value carries no methods.
- **Variadic params:** `...name: T` collapses to a `List[T]` slot (Go/Swift `T...`). New `Token::DotDotDot`
  (lexer emits on a third `.`); `ast::Param.is_variadic` (runtime-inert like `is_ref`); `parse_params`
  gates it on `allow_defaults` (free fns / methods / native decls yes; closures / extern / protocol sigs
  no) and enforces ≤1 variadic, element-type-required, no-default. `FnSig.variadic: Option<usize>` +
  `fn_sig`/`harvest_native_fn_sig`/`register_native_decl` wrap the slot in `List[T]`. **Mechanism (the
  deliberate refinement over "compiler lowering + interp mirror"): the surplus positionals are collapsed
  into a synthesized `List` literal in the DESUGAR pass (`normalize_call`)** — the parity-by-construction
  seam — so the compiler AND interp need ZERO changes and VM==interp is automatic. Everything after the
  variadic is keyword-only (post-variadic param with a default = optional kw arg, without = required kw
  arg); a positional can never land in a keyword-only slot (all trailing positionals are swept). Collapse
  runs on desugar pass 1 only (idempotency — pass 2 would double-wrap). `PSpec.is_variadic` threaded
  through every spec builder. `examples/variadic.chz` golden asserted byte-identical on interp / --serial
  VM / M:N VM.
- **`print` port:** now declared `native fn print(...args: Any, sep: str = " ", end: str = "\n") -> nil`
  in `std/prelude.chz` (harvested into `native_prelude_sigs`), retiring `sig_print()` + the
  `builtin_container_sig` print special-case — the LAST synthetic Rust signature. Lowering is UNCHANGED:
  a direct `print(...)` still compiles to `Op::CallPrint`/`CallPrintSep` byte-identically (the file-backed
  decl is checker-only name/sig authority). The VALUE form (`p := print`) stays a FIXED 1-arg
  `Ty::BuiltinFn` via an `infer_ident` special-case (the specialized opcodes are unreachable through a
  bound value — a design-sanctioned split, not a gap); the existing `print` value-form / `defer` / `spawn`
  tests stay green.
- **Deferred (docs only):** `cast[T](val: Any) -> Option[T]` checked downcast — design + runtime-erasure
  policy recorded in `docs/future.md §3.14` (parameterized targets like `cast[List[int]]` unsound until
  runtime type tags exist). cFFI stays fixed-arity (`Any` does not feed the C vararg ABI — `docs/ffi-and-
  packaging.md §5`).
- **Heterogeneous args into `...xs: Any` (and `List[Any]`) — supported (the honest top-type element).**
  `fn describe(...xs: Any)` called `describe(1, "a", true)` collapses to a `List[Any]` and type-checks
  clean: every value vacuously satisfies the empty `Any` protocol. The synthesized variadic `List`
  literal (and any annotated `xs: List[Any] = [1, "a", true]`) is now checked **expected-type-directed** —
  the declared `List[E]` element type is driven onto each element (`Checker::infer_list` takes the
  `expected_hint`; when every element is assignable to `E` it types as `List[E]`, bypassing bottom-up
  sibling unification). Falls back to the bottom-up "list elements differ" diagnostic + untyped-int-CONSTANT
  →float widening when `E` is NOT satisfied-by-all (so `List[int] = [1, "a"]` still errors). Golden:
  `examples/variadic.chz` (`describe(1,"a",true)` → `3`, `zs: List[Any] = [1,"a",true]` → `3`), byte-
  identical on interp / --serial / default. Tests: checker `variadic_any_accepts_heterogeneous`,
  `list_any_annotation_accepts_heterogeneous`, `list_int_annotation_still_rejects_heterogeneous`.
  (Adversarial-review fix: the earlier collapse synthesized a bare `List` literal that the checker
  inferred bottom-up, rejecting heterogeneous `Any` args — the exact opposite of `Any`'s purpose.)
- **Known v1 tradeoffs:** a heterogeneous variadic arg into a NON-top element slot (`f(1, "x")` into
  `...xs: int`) surfaces as a
  `List`-literal element-type error, not a precise per-arg message (still a compile error). A variadic fn
  used as a VALUE takes the collapsed `List[T]` slot (`g([1,2,3])` works, `g(1,2,3)` does not) — mirrors
  `print`'s fixed value form. A variadic CALL used as a parameter/field **default**
  (`fn g(x: int = sum_all(1,2,3))`) is **not** collapsed and so is a **compile error** (the desugar
  collapse runs on pass 1 only for idempotency; a default is spliced after pass 1) — it fails identically
  on both engines (a compile error, NOT a parity divergence). Wrap the default in a fixed-arity helper.
  Narrow enough to defer; a robust fix needs a per-call "already collapsed" marker.
- **Fix (adversarial-review follow-up):** a variadic METHOD call (`recv.m(a,b,c)`) is now collapsed even
  when another struct/enum defines a method of the SAME name with a DIFFERENT param list (a fixed-arity
  sibling, or two variadics differing only in the variadic param's NAME). The desugar method-spec
  resolution is now **receiver-aware first**: when the receiver's struct type is statically knowable
  (a let-bound local, an inline ctor, a struct-returning fn, or now a **typed parameter** `x: A`), it
  binds `m` against THAT struct's exact spec (incl. its variadic index) before the name-keyed all-agree
  table — so the surplus positionals collapse instead of reaching the checker uncollapsed and being
  rejected against the single `List[T]` slot. Typed params are registered in desugar's `local_struct`
  (`bind_param`), so a keyword-only post-variadic tail on a name-colliding method (`a.m(1,2,flag=true)`)
  also resolves rather than emitting the unsatisfiable "pass arguments positionally" error. A named call
  on a KNOWN receiver now resolves receiver-aware too (previously errored "multiple structs"); an
  UNRESOLVABLE receiver (unannotated closure param) still errors clearly. Regression tests: checker
  `variadic_method_*`, interp two-engine `variadic_method_name_collision_runs_byte_identical`,
  desugar `ambiguous_method_named_*`. **Lexer surface changed (`...` token):** editor TextMate grammar regenerated
  (`UPDATE_EDITOR_ASSETS=1 cargo test --test editor_tmlanguage`); **manual follow-up:** reinstall
  `chezzi-lsp` so editors stop serving stale highlighting. Docs updated: `spec.md` (variadic NON-GOAL
  overturned for arguments; variadic generics stay a non-goal), `syntax.md`, `stdlib.md`, `grammar.bnf`
  (`<param>` variadic alt), `ffi-and-packaging.md §5`, `future.md`.

**✅ LANGUAGE — conditional methods: `where` on a user struct/enum/newtype method's RECEIVER type
param (2026-07-03).** Closes the consistency gap left by the `where`-clause entry below: a user *method*
may now `where`-bound the ENCLOSING type's own type parameter (`struct Box[T]: fn top(self) -> T where
T: Comparable`), making the method callable only when the receiver's concrete type argument satisfies
the bound — Rust's `impl<T: Ord> Box<T>` conditional methods, and parity with native `List[T].sort`/`sum`.
**Mechanism (checker-only, additive):** `fn_sig`'s `where`-merge loop, when a `where` entry names
neither the method's own `[U]` nor an unknown param but IS the enclosing type's param (present in
`self.type_params`), records it on `receiver_bounds` → carried on the returned `FnSig.where_bounds`
(the SAME field the native harvest uses) instead of erroring; the struct/enum/newtype INSTANCE
method-call dispatch arms then call `enforce_bounds(&sig.where_bounds, {structParam_i → concreteArg_i},
span)` — byte-for-byte like the native `Ty::List` arm's `{T → elem}`. The instance enforcement fires
AFTER the `is_static` rejection (a static method wrongly called on a value yields ONLY the single
static-method diagnostic, no spurious bound error). A no-`self` (static) method carries a receiver
`where` too, so `infer_static_call` (the `Type.method(…)` path) ALSO enforces `sig.where_bounds`
against the inferred enclosing-param substitution — a conditional factory `Box.of(q)` rejects a
non-satisfying `q` at check time (closes the static accept-without-enforce hole). A `where` naming
NEITHER an own nor a receiver param still errors "unknown type parameter"; a method's OWN `[U]`
`where` still merges as before. Newtype is included for soundness (shared `fn_sig` accepts
receiver-bounds for newtype methods too → enforce at all three instance arms, no accept-without-enforce
hole). **Unknown/late-usage:** reuses
`enforce_bounds` verbatim — `satisfies_args_d` returns Ok for `Ty::Unknown` ("don't cascade"), so a
still-unpinned receiver arg DEFERS (never a spurious "does not satisfy"); a genuinely never-pinned
binding still fails at the pre-existing "cannot infer element type" error. **Three-engine byte-identical:**
`where` lowers to NOTHING — `src/interp`/`src/vm` get ONLY additive golden tests; `examples/conditional_
method.chz` (Box/Opt/Stack conditional methods invoked+printed, plus a `max2` whose body USES the
bound) asserted byte-identical on interp / --serial VM / M:N VM (`golden_conditional_method_chz` +
`..._matches_expected_and_interp`). No grammar change (`where` already grammatical on methods —
`cargo test conformance` green). checker + docs + additive golden tests only.
**Three follow-up fixes (adversarial review):** (1) the conditional method BODY may now use the bounded
op — `check_fn_body` merges `sig.where_bounds` onto the in-scope ENCLOSING param for the body's
duration (was recorded call-site-only, so `self.val < other` errored `cannot compare T and T`),
restoring symmetry with the free-fn `where` path (test `conditional_method_body_uses_receiver_bound`
+ enum mirror). (2) `fn_sig` DEDUPs the receiver-bound against the enclosing param's DECLARED bounds
(`struct Box[T: Comparable]` + `where T: Comparable`), so the static-dispatch path — which enforces
both `tps` and `sig.where_bounds` — no longer emits the identical "does not satisfy" twice (test
`conditional_static_method_redundant_decl_bound_reports_once`). (3) **SOUNDNESS — conditional
CONFORMANCE.** A conditional method that *is* a protocol's required method (e.g. `compare` ⇒
`Comparable`) makes the type STRUCTURALLY satisfy that protocol; enforcing the receiver `where` only
at explicit method-call dispatch left every `satisfies`-based consumer (operator dispatch `<`/`+`,
generic bounds `[U: Comparable]`, protocol-typed params) BYPASSING it — `Box[Tag] < Box[Tag]`
check-passed then ran into `Tag has no compare` (check-ok/run-diverge). Fixed at the source:
`satisfies_methods` now, after `method_matches`, verifies each candidate method's `where_bounds` hold
under the querying type's `{structParam → concreteArg}` map (`self.satisfies_args`, so `Ty::Unknown`
defers exactly like the call-site path). Conditional conformance is now honoured EVERYWHERE — `Box[int]`
is `Comparable`, `Box[Tag]` is not — closing the operator/bound/param bypass (tests
`conditional_method_operator_dispatch_enforces_receiver_bound`,
`conditional_method_as_generic_bound_arg_enforces`). Low blast radius: pre-conditional-methods code has
no method `where_bounds`, so the new check is a no-op for all existing structural conformance.

**✅ LANGUAGE — `where`-clause generic bounds + file-backed List `sort`/`sum` port (2026-07-03).** Adds
`where T: Bound, …` as an alternative spelling of generic bounds after a fn/native-fn signature
(`fn max[T](a: T, b: T) -> T where T: Comparable`), then USES it to finish the phase-5a container port:
`sort` is now fully file-backed as `native fn sort(self) -> nil where T: Comparable` in `std/prelude.chz`
and its bespoke Comparable arm in the `Ty::List` method-dispatch is DELETED (Comparable's satisfaction set
exactly matches runtime sort capability — fully sound); `sum` gains a `where T: Add` annotation (documentation
of a necessary bound) while its residual `!elem.is_numeric()` check-gate SURVIVES (Option B — sum's true
requirement is MONOID = Add + zero for the empty-list case, both runtimes numeric-only, so `where T: Add`
alone is too broad; a struct with a structural `add` still errors at CHECK time). **Mechanism:** new
`Token::Where` (KEYWORDS, corpus-safe) + `parse_where_bounds()` (reuses `parse_bound`) attaching an additive
`where_bounds: Vec<TypeParam>` to `FnDecl`/`NativeDecl`; for USER fns `fn_sig` MERGES each `where` entry's
bounds into the matching `[T]` type param (unknown-param = clear error; body-check enters the merged params so
a `where`-bounded op like `<` works in the body), so the existing `infer_generic_call`→`enforce_bounds` path
handles call sites with ZERO new machinery; for NATIVE methods `harvest_native_fn_sig` carries `where_bounds`
onto the sig and the `Ty::List` arm calls `enforce_bounds(&sig.where_bounds, {T->elem})`. (A user METHOD
where-bounding the receiver struct's OWN param — the "conditional method" shape — was subsequently
SHIPPED; see the conditional-methods entry above.) **BEHAVIOR-PRESERVING / three-engine byte-identical:** `where` lowers to NOTHING at runtime —
`src/interp` is UNTOUCHED and `src/vm` gets only a golden test; `examples/where_sort_sum.chz` (sort int/float/
struct-with-`compare`, sum int/float) is asserted byte-identical on interp / --serial VM / M:N VM
(`golden_where_sort_sum_chz_matches_expected_and_interp`). `docs/grammar.bnf` gains `<whereClause>`/`<whereList>`
(WHERE terminal + `parse_where_bounds` mapped in conformance; `tests/corpus/accept/where_clause.chz`). The
sort-arm-DELETED changes two existing tests' expected message from the bespoke `sort() requires …` to the
standard `does not satisfy Comparable` bound diagnostic. Lexer+parser+ast+checker+prelude+grammar+docs; both
engines untouched.

**✅ NATIVE-PRELUDE — phase 5c-protocols COMPLETE (all 16 builtin/reserved PROTOCOLS declared in
`std/prelude.chz` as plain `protocol` decls, a drift-guarded ADDITIVE mirror of the Rust seed)
(2026-07-03).** `Iterable[Elem]` (`iter(self) -> Iterator[Elem]`) lands as the 16th and last file-backed
protocol, closing the 5c port — its return type resolves via `resolve_type`'s dedicated `Iterator[T]`
value arm to the same `Ty::Struct("Iterator",[Elem])` the seed uses, so its shape byte-matches like the
other 15 (the earlier "parameterized-protocol-return rejected by `resolve_type`" claim was inaccurate; no
resolve fix was needed — the seam already yields the seed shape). The reserved structural protocols' SHAPE
(method sigs + `+`-joined embeds) lives in
`std/prelude.chz` — `Comparable`/`Stringable`/`Error`/`Hashable`, the operator protocols `Add`/`Sub`/`Mul`/
`Div`/`Mod`/`Neg`, the `Arithmetic` bundle (`: Add + Sub + Mul + Div`), `Iterator[Elem]`
(`next(self) -> Option[Self]`; Elem arity-only), `Iterable[Elem]` (`iter(self) -> Iterator[Elem]`), and
`Index[K,V]`/`IndexSet[K,V]`/`Slice[R]` — using the
EXISTING `protocol` decl syntax (no new grammar). **DRIFT-GUARDED PARTIAL PORT (the phase-5b precedent —
SHAPE moves, WIRING stays; the task's documented fallback, logged):** `prebuilt_protocols()` STAYS the live
runtime source (seeded at `Checker::new`, before any module); the `.chz` decls are NEVER inserted into the
protocol table — `hoist_protocol`'s reserved arm is now VALIDATE-AND-NO-OP in a stdlib module (early return,
no insert, no error), keeping the user-module `reserved (builtin)` rejection unchanged. Everything that
DECIDES conformance + operator binding stays 100% Rust-wired and UNTOUCHED: `satisfies` (int/float satisfy
`Add`/`Comparable`/`Neg` INTRINSICALLY with no method; a user struct satisfies structurally via its methods),
`iter_elem`/`iterable_elem` (Iterator/Iterable conformance), `recover_index_args`, operator lowering
(`+`→add, `<`→compare, `for`→Iterator, `[]`→Index, `[:]`→Slice, `?`/interpolation/Map-keys), `check_bounds`,
`is_reserved_protocol`, and BOTH engines (`src/vm`/`src/interp` are UNTOUCHED — checker-only, decl-shape-only,
so 3-engine parity holds by construction). **What the `.chz` decls buy:** a checked source-of-truth MIRROR
of each protocol shape, pinned to the Rust seed by `assert_native_protocol_shape_matches` (a debug-only
always-on `harvest_protocol_shape` + `debug_assert_eq!`/`fn_sig_eq` on the always-linked prelude — assert-only,
resolution-inert, keeps the harvest helper production-live) and by the unit guard
`native_protocol_shapes_match_prebuilt_seed` (harvested `type_params`/`embeds`/ordered method sigs
byte-equal `prebuilt_protocols()`). **COUNT:** 16 reserved protocols at this 5c milestone; ALL now file-backed +
drift-guarded (Iterable no longer the exception). Grew to 18 later (`Any`, then `Convert`). Runtime `Iterable` satisfaction (`iterable_elem`/
`iter_elem`, for-loop lowering, `infer_method_call`'s `Iterable.iter` element recovery) is UNTOUCHED — the
`.chz` decl is validate-and-no-op at hoist, never inserted into the protocol table. **BEHAVIOR-PRESERVING / three-engine
byte-identical:** `examples/protocols_5c.chz` (int/float intrinsic arithmetic, a user 4-op struct under
`+ - * /` AND through `[T: Arithmetic]`, `[T: Comparable]` max over a Comparable struct, a user `Iterator`
struct in a `for`, builtin Index/Slice, a user IndexSet struct) is asserted byte-identical on interp /
--serial VM / M:N VM (`protocols_5c_3engine_parity` via `assert_mc_parity`), and every pre-existing
protocol/bound/operator-overload/generic-constraint/Iterator test stays green UNCHANGED. `grammar.bnf`
needs NO change (plain `protocol` already in the grammar; conformance green). Checker+prelude+docs only.

**✅ NATIVE-PRELUDE — phase 5b-native-enum (the builtin `Option`/`Result` variant SHAPE made
file-backed: `native enum Option[T]` (`Some(T)`/`None`) / `native enum Result[T, E]` (`Ok(T)`/`Err(E)`)
declared in `std/prelude.chz`, mapped ADDITIVELY onto the reserved `Ty::Option`/`Ty::Result`)
(2026-07-02).** Builds the ENUM analog of `native struct` — a new `native enum NAME[T…]:` decl form
(parser `parse_native_enum` + `StmtKind::NativeEnum` + hoist reject in user modules; body-less variants
via `parse_enum`'s variant loop, generics via `parse_type_params`, optional body-less `native fn`
methods with a leading bare `self` harvested like native-struct methods, no-self = parse error) — and
uses it to file-back the declarable variant SHAPE of the two most deeply-wired builtin enums.
**PARTIAL PORT (the task's documented fallback outcome — logged): SHAPE moves, WIRING stays.** Unlike
5a/4c (which retired a LIVE, consulted `*_method_sig` arm and rerouted resolution through the harvested
table), Option/Result have (a) ZERO bespoke methods (no `Ty::Option`/`Ty::Result` arm in method
resolution) and (b) NO variant-table consumer — their variant shape is synthesized INLINE from the `Ty`
shape at ~8 Rust sites (`variants_of`, `match_kind`, the `Ok`/`Err`/`Some`/`None` name-guards +
construction, `resolve_type` identity), none of which read `self.enums`. Rerouting those through a
harvested table IS touching the `?`/match core the phase must keep byte-identical, so **nothing of the
wiring moved**: `?` propagation (Result AND Option), match exhaustiveness, `Ok`/`Err`/`Some`/`None`
construction (checker + `NativeRet` runtime), the `Result[T]`→`E = Error`-protocol surface default, and
top-level error unwind ALL stay 100% Rust-inline and UNTOUCHED. **What the `.chz` decl buys:** a checked
source-of-truth MIRROR of the variant shape, guarded against drift by `assert_native_enum_shape_matches`
(a `harvest_native_enum_table` + `debug_assert_eq!` on the always-linked prelude — assert-only, no
resolution effect, keeps the harvest helper production-live) and by the unit guard
`native_enum_option_result_shape_matches_inline` (parsed+resolved variants byte-equal
`variants_of(Ty::option/result_e(Param))`). The `NativeEnum` hoist arm CRITICALLY must NOT register into
`self.enums`/`enum_names` (that would mint a colliding nominal `Ty::Enum` and silently break `?`/match) —
it stays validate-and-no-op; identity stays 100% in `resolve_type`. **BEHAVIOR-PRESERVING / three-engine
byte-identical:** `src/vm` + `src/interp` gain ONLY forced no-op AST match arms; `examples/native_enum_smoke.chz`
(construction + `?` on a Result- and an Option-returning fn + exhaustive match) is asserted byte-identical
on interp / --serial VM / M:N VM (`golden_native_enum_smoke_chz_matches_expected_and_interp`), and every
pre-existing Option/Result/`?`/match/exhaustiveness test stays green UNCHANGED. New `nativeEnumDecl`
production in `grammar.bnf` (conformance green, corpus `accept/native_enum.chz`). Parser+checker+docs only.
`Iterator` (a protocol, NOT an enum) is untouched here — its protocol SHAPE is file-backed in phase 5c
(see above); conformance stays `iter_elem`-special.

**✅ NATIVE-PRELUDE — phase 5a-containers (the builtin `List`/`Map`/`Set` METHOD surface made
file-backed: `native struct List[T]` / `Map[K, V]` / `Set[T]` declared in `std/prelude.chz`, harvested
into method tables mapped ADDITIVELY onto the reserved `Ty::List`/`Ty::Map`/`Ty::Set`) (2026-07-02).**
The three builtin containers' METHOD sigs move out of the bespoke Rust `list_method_sig`/`map_method_sig`/
`set_method_sig` arms into body-less `native fn` methods (leading bare `self`, stripped by the harvest) on
`native struct` decls in the always-linked `std/prelude.chz` — the exact phase-4c-concurrency generic
native-struct + harvest pattern, now applied to the RESERVED UNIVERSE containers. **BEHAVIOR-PRESERVING:**
each harvested `FnSig` BYTE-MATCHES the retired arm (guarded by `container_method_sigs_byte_match`
enumerating all 24 flat methods with concrete K/V subst), and output is byte-identical on all three engines
(guarded by `examples/container_methods.chz` + `container_methods_3engine_parity`). **CRITICAL additive
subtlety (as concurrency/net):** `List`/`Map`/`Set` KEEP resolving to the reserved `Ty::List`/`Ty::Map`/
`Ty::Set` — the harvest attaches ONLY the method table (never a fresh `Ty::Struct`); the LITERAL syntax
(`[...]`/`{k:v}`/`{1,2}`) + the turbofish ctor (`List[int]()`, `builtin_container_sig`) + `resolve_type`'s
element-type arms stay 100% COMPILER-WIRED and UNCHANGED, and **runtime (`src/vm`/`src/interp`) is
UNTOUCHED** (method dispatch stays by name). Seeding follows the `ref_seed`/`concurrency_seeds` precedent:
the prelude's `List`/`Map`/`Set` tables are captured into a new `container_seeds` field when the prelude
module (graph order[0], always-linked) is checked, and `seed_stdlib_structs` re-seeds them bare (method-
table only — NO `struct_names`/`bare_types` licensing needed, they're UNIVERSE) into `self.structs`; the
cfg(test) single-module `check` path harvests them straight in via `seed_native_prelude_sigs`. The
`Ty::List`/`Ty::Map`/`Ty::Set` dispatch arms route through `native_handle_method` with the value's
element/key/value type substituted for `Ty::Param`. **The generic-recovery `List` HOFs are now ALSO
file-backed (UPDATE 2026-07-03, phase 6 — closure-return loop-back):** `map[U]`/`filter`/`fold[U]`/
`sort_by`/`sort_by_key[K: Comparable]` are declared in the prelude struct; the bespoke `infer_list_hof`
arm is DELETED. This needed two generalizations: (1) a **native method may declare its OWN `[U]` type
param** after the name (parser `parse_native` + AST `NativeDecl.type_params` + harvest onto
`FnSig.type_params`; grammar `nativeDecl`/`nativeMethodDecl` gain optional `<typeParams>`), so a
method-own param routes through `infer_generic_method`; (2) the generic solver gained a **closure-return
LOOP-BACK** — after the per-arg re-inference pass (which pins an unannotated closure's params and computes
its concrete body-return), `check_generic_arg` now RETURNS the refined actual type, and
`infer_generic_method` feeds those refined types into a SECOND `unify` pass, filling ONLY params still free
after pass 1 (safe because `unify` is only-bind-unbound + ignore-Unknown → every already-resolved generic
call is a strict no-op), then re-enforces bounds on the newly-bound params and degrades any still-free
param to `Unknown` **only when it appears in a PARAMETER position** (recoverable-in-principle but the
argument's type was itself `Unknown` — the empty-collection case `[].map(...)` → `List[?]`). A param
appearing ONLY in the return position and in NO parameter (`fn make[U](self) -> U`) is genuinely
un-inferable and is deliberately LEFT as a leaked `Ty::Param`, so assigning the result to a concrete type
is REJECTED (soundness: a wrong static type must not silently escape onto the value — an unconditional
degrade to `Unknown`, which `assignable` treats as universally assignable, would mask it). Recovers a
return-position param from an unannotated closure body generally (not
map-special): `Box(3).apply(fn(x): x+1)` on `fn apply[U](self, f: fn(T) -> U) -> U` also yields `int`.
Diagnostics are the uniform general-path wording (retired the bespoke "predicate"/"map expects…"/
"sort_by_key key type must be Comparable" strings). `sort` stays file-backed via `where T: Comparable`;
`sum` KEEPS its `!elem.is_numeric()` residual gate (Monoid requirement, `where T: Add` alone too broad).
Checker/parser-only; runtime type-erased + name-keyed → 3-engine byte-identical parity. `Map`/`Set`'s key/element type param carries a `Hashable` bound so the internal
`Map[K, V]`/`Set[T]` return types resolve past the hashable gate at harvest. The bespoke
`list_method_sig`/`map_method_sig`/`set_method_sig` fns are DELETED; `unique_member_owner`'s bail set now
checks the harvested tables' `methods.contains_key` (byte-identical to the retired arms' 9/8/7 flat
methods) and the `builtin_method_slices_all_resolve` hover drift-guard resolves the slices against the
seeded tables. Parser+checker-only. ~1283 checker tests + 3-engine parity green.

**Bug D fix (2026-07-04 — closure-return loop-back now recovers a method `[U]` through a NESTED FREE
generic call in the body).** `xs.map(fn(x): ident(x))` where `fn ident[T](x: T) -> T` was spuriously
rejected (`cannot apply + to T and int` on `ys[0] + 1`): the closure param `x` inferred `int`, but the
UNANNOTATED body `ident(x)` was prepass-inferred under `generic_arg_prepass` (`x: Unknown`), so
`infer_generic_call(ident, [Unknown])` could not bind ident's own `T` and returned a LEAKED
`Ty::Param("T")`. Pass-1 `unify` in `infer_generic_method` then prematurely pinned `map`'s return-position
`U := Param("T")`, and the loop-back — which only fills params still FREE — could not correct it, so
`ys: List[T]` leaked. FIX (checker-only, `src/checker/proto.rs` + a `mask_closure_ret` helper in
`src/checker/mod.rs`): in `infer_generic_method`, when the arg is a closure **with NO return annotation**,
(1) unify pass-1 against a RETURN-MASKED copy of its actual `Func` (return → `Ty::Unknown`) so only its
PARAMETER positions can bind a method type param, and (2) ALWAYS mask the same closure's fallback return in the
`check_generic_arg` assignability check (the prepass leaked `Param` would otherwise mismatch `want`'s return —
whether that return is a still-free `[U]` OR already concrete), keeping the internal check to params + arity.
This defers `U` to the loop-back's checking-mode re-inference, which recovers it as the CONCRETE return (`int`)
→ `ys: List[int]`, prints `2`. SOUNDNESS is upheld by a SEPARATE explicit check, not by the mask's presence:
after `check_generic_arg` returns the REFINED (checking-mode re-inferred) closure type, when the closure's
expected return is ALREADY concrete (e.g. `fold[U]`'s `U` pinned to `int` by `init`) the refined return is
asserted assignable to it — so `xs.fold(0, fn(acc,x): "wrong")` is rejected (`str` ≠ `int`) while
`xs.fold(0, fn(acc,x): ident(x))`/`ident(acc)` — whose prepass leaked a rigid `Ty::Param` but whose refined
body types `int` — is ACCEPTED. (The earlier gate that masked only a still-free `[U]` (`closure_ret_wants_free_mtp`)
was WRONG: it spuriously rejected exactly those concrete-return nested-generic-call `fold` bodies — the
adversarial-review-caught regression — because the unmasked prepass `fn(?,?) -> T` failed the internal check.
Checking the refined type fixes both directions.) `U` is bound concretely, never degraded to `Unknown`
(assigning the result to `List[str]`/`List[List[int]]` is still cleanly rejected). An annotated closure return
(`fn(a,b) -> int: …`) is left authoritative (no mask), preserving the exact arity-mismatch diagnostic. Runtime is generic-erased → serial==M:N automatic.

**Bug D FREE-FN analog fix (2026-07-05 — the same closure-return recovery now runs on the generic
FREE-FUNCTION / module-qualified-fn HOF path).** The Bug D fix above landed only on the METHOD path
(`infer_generic_method`); the symmetric `infer_generic_call` deliberately DISCARDED the refined closure
type (`let _ = self.check_generic_arg(...)`), so a user free-fn HOF with a **return-only** type param
leaked `Ty::Param` into its return — `fn applyone[U](x: int, f: fn(int) -> U) -> U` called
`applyone(5, fn(x): x*2)` then `+ 1`, and the `-> List[U]` container form `mymap([1,2,3], fn(x): x*2)`,
both rejected with `cannot apply + to U and int`; the sibling-pinned `fn apply[A,B](f: fn(A)->B, a: A)
-> B` (`apply(fn(x): x*2, 5)`), the protocol-bounded `fn mapadd[U: Add](...)`, and nested-free-generic
bodies (`fn(x): ident(x)`) likewise. FIX (checker-only, `src/checker/proto.rs`): Bug D's FINAL sound
mechanism (return-masked pass-1 unify + REFINED-type capture + the SEPARATE concrete-return soundness
check + the loop-back second `unify` + newly-bound bound re-enforcement + the method-only param-position
degrade) is factored into ONE shared helper `recover_return_only_params` called by BOTH
`infer_generic_method` (a byte-identical refactor — the existing Bug-D method tests are the safety net)
and `infer_generic_call`. The free-fn path additionally masks bare-closure returns in its pass-1
`unify` loop (mirroring the method path) so a nested-free-generic body's leaked prepass `Param` cannot
prematurely pin the return-only param before the loop-back. **Two adversarial-review bugs fixed on a
follow-up pass (2026-07-05):** (bug 1) the free-fn path's un-inferable-param probe
(`report_uninferable_closure_params`) runs BEFORE the loop-back, so a return-only `[T]` bound only from
a bare closure's CONCRETE return was still masked-away and mis-reported as a deadlock when a SIBLING
closure used the same `[T]` in PARAMETER position (`fn pair[T](f: fn()->T, g: fn(T)->int)` called
`pair(fn(): 5, fn(x): x+1)` — accepted on `main`, wrongly rejected on the branch). FIX: a small
concrete-return sub-pass right after pass-1 binds `[T]` from any bare closure whose prepass return is
already concrete (`ty_contains_param` FALSE) — AFTER value/param args (only-bind-unbound `unify`, so a
sibling value arg still wins, no binding race) and BEFORE the probe; a leaked-`Param` prepass return
stays masked/deferred to the loop-back. (bug 2) two closures binding the SAME return-only `[U]` to
CONFLICTING concrete types type-checked OK but bound `[U]` from only the first, dropping the second
(`fn pick[U](cond, a: fn()->U, b: fn()->U)` / `fn two[U](f: fn(int)->U, g: fn(int)->U)` with a `str` vs
`int` pair — accepted then crashed at runtime). FIX: the loop-back `unify` is now INTERLEAVED into the
per-arg loop, so once the first closure binds `[U]` the sibling's `want` return is CONCRETE and its
mismatching body is rejected by the SEPARATE concrete-return soundness check instead of being silently
dropped. IMPORTANT (adversarial-review fix): the
final param-position degrade-to-`Unknown` step is **gated `true` on the METHOD path only** (its
receiver-collection HOFs `[].map(...)` intentionally degrade an empty element param to `List[?]`) and
**`false` on the free-fn path** — `infer_generic_call` never degraded, so a still-unbound param-position
free-fn type param bound to nothing by an empty-collection arg (`fn first[U](xs: List[U]) -> U` called
`first([]) + 1`, or `fn tag[U](xs: List[U]) -> List[U]`) must stay a leaked `Ty::Param` that downstream
concrete use REJECTS and that keeps the deliberate Category-2 "un-inferred type parameter; bind at the
construction site" diagnostic; degrading it there laundered a compile error into a runtime panic and is
NOT this change's scope. Free-fn CLOSURE-param type params left un-inferable by an empty arg are already
`Unknown`-bound by `report_uninferable_closure_params`, so skipping the degrade there is
behavior-preserving. SOUNDNESS is upheld by the same SEPARATE check, not the mask:
when the return-only param is ALREADY pinned by a sibling value arg / explicit slot, the refined closure
return is asserted assignable to that pin, so `fn f[U](init: U, g: fn(int) -> U, ...)` called
`f(0, fn(x): str(x), ...)` is a clean type error (`closure argument to 'f' returns str, expected int`),
never laundered onto the pinned `int` — the free-fn analog of the `fold`-init laundering hole. A
genuinely un-inferable return-only param (`fn make[U]() -> U`) stays a leaked `Ty::Param` (concrete
assignment rejected); a genuinely ambiguous body (`fn(x): fn(y): x+y`) stays exactly one
`cannot infer type of parameter 'y'` error. OUT OF SCOPE (unchanged): the ctor paths
(`infer_generic_struct`/`infer_newtype_call`) share the identical discard but have no free-function
repro — a possible follow-up; Category-2 late/backward inference and the generic-fn-VALUE gap
(`g := ident; g(5)`) are distinct limitations, untouched. +7 checker tests (recovers scalar+container,
pinned-mismatch-rejected, boundaries, must-not-regress, ambiguous-stays-clean, empty-arg-stays-rejected,
plus the two adversarial-review regressions: sibling-closure-param-use-recovers [bug 1] and
conflicting-return-only-closures-rejected [bug 2]) + 3 parity tests
(`parity_free_fn_hof_map`/`_apply_sibling`/`_sibling_closure_param`), all RED-first on the release
binary. Runtime is generic-erased → serial==M:N automatic.

**✅ NATIVE-PRELUDE — phase 4c-followup (native instance methods now declare `self`, mirroring user
structs) (2026-07-02).** A `native fn` inside a `native struct` body is an INSTANCE method and now MUST
declare a leading bare `self` as its first parameter (`native fn read(self, n: int) -> Result[str]`,
`native fn get(self) -> T`) — resolving the DX asymmetry where native methods omitted `self` yet were
instance methods. **BEHAVIOR-PRESERVING:** the parser accepts `self` and harvest (`harvest_native_fn_sig(_,
skip_self=true)` in PASS 1b) STRIPS it BEFORE the param→`Ty` map (so `self` is never a spurious dynamic
`Ty::Unknown` receiver) AND before the optional-tail count — the resulting method-table `FnSig`
(params/min_params/ret) is BYTE-IDENTICAL to the pre-`self` spelling, so checker resolution, runtime
dispatch, and 3-engine parity are all unchanged (the existing `net_sig_from_file_not_native_module_sig` /
`concurrency_harvested_method_sigs_shape` sig-guards pass with their SAME asserted params — the
behavior-preserving proof). A self-less body `native fn` is now a parse error (`native instance method
must declare 'self' as its first parameter`) — the self-less form is **RESERVED** for a future native
STATIC method (not implemented — just the error). `self` is valid ONLY as the first param, and a
module-level (free) `native fn` may NOT take `self` (`parse_native(in_struct: bool)` threads the rule).
Updated `std/net.chz` (Socket/Listener, 6 methods) + `std/concurrency.chz` (Shared/RwShared/Atomic/
Executor); `regex.Match`/`request.Response` are fields-only (no change). Parser+checker-only; `src/vm` +
`src/interp` UNTOUCHED.

**✅ NATIVE-PRELUDE — phase 4c-concurrency (`std.concurrency` made file-backed: the four GENERIC native
types `Shared[T]`/`RwShared[T]`/`Atomic[T]`/`Executor` WITH method tables declared in
`std/concurrency.chz`) (2026-07-02).** The **LAST** virtual native module — after it EVERY native std
module is file-backed, and `native_module_sig` retains only the **`ffi`** (`ptr` + fixed-width names) +
**`time`** (`timer`) opcode/type-license tails (the `concurrency` arm is **DELETED ENTIRELY**, no
residual). This EXTENDS the 4c-net native-method-binding capability from non-generic native structs
(`Socket`/`Listener`) to **GENERIC** ones: a `native fn` in a `native struct Shared[T]` body harvests a
method sig carrying `Ty::Param("T")`, and at each call site `native_handle_method(ty, method, &[elem])`
**substitutes** the box's element type (`Shared[int].set` expects `int`) — the same per-type param subst
the generic-struct machinery uses. **CRITICAL additive subtlety (as net):** `Shared`/`RwShared`/`Atomic`/
`Executor` KEEP resolving to the RESERVED `Ty::Shared`/`Ty::RwShared`/`Ty::Atomic`/`Ty::Executor` (opaque
VM handles — NOT fresh `Ty::Struct`); the `.chz` `native struct` feeds the checker ONLY the type + method
sigs, the ctors STILL lower to `Op::NewShared`/etc **by name** and every method stays VM-intercepted —
**runtime UNTOUCHED**. The harvested tables are cached into `concurrency_seeds` (AFTER
`attach_native_module_metadata`, unlike net's before, because the metadata step mutates the read/submit
sigs) and re-seeded bare into `self.structs` by `seed_stdlib_structs` (method-table only — NO
`struct_names`/`bare_types` licensing, so the bare name stays import-gated by `imported_concurrency`).
**Two sigs a plain harvest can't express, ported as metadata** in `attach_native_module_metadata`:
`RwShared.read(f)` — declared UNANNOTATED, retyped to `fn(T) -> ?` (any R; the real R is recovered from
the closure at the `Ty::RwShared` dispatch arm); `Executor.submit(f)` — declared UNANNOTATED, retyped to
`fn() -> ?` (any return, zero-arity). **One dispatch-time residual:** `Atomic.add`/`sub` exist only for a
numeric `T` — a `!elem.is_numeric()` gate kept in the `Ty::Atomic` arm. The bespoke
`shared_method_sig`/`rwshared_method_sig`/`atomic_method_sig`/`executor_method_sig` fns are DELETED.
**Qualified-path fix (new vs net):** the harvested `sig.struct_defs` entry made `concurrency.Shared[int]`
(a qualified annotation / `type`-alias / `newtype` body) resolve as a nominal `Ty::Struct` — both
`resolve_type` and `resolve_qualified_ro` now skip a reserved native type (`qualified_builtin_ty` is
`Some`) so it keeps its reserved `Ty` (matching the bare-after-import path). **Stdlib consumers:**
`std/concurrency/collection.chz` (RwShared) + `std/cancel.chz` (Shared) now explicitly `import
std.concurrency` — the file-backed native module must be a graph DEPENDENCY so its method table is
harvested/seeded before those modules are checked, regardless of the entry program's import order
(behavior-preserving: the bare names were already stdlib-licensed). Tests:
`concurrency_sig_from_file_not_native_module_sig` (arm gone, four types harvested with method names),
`concurrency_harvested_method_sigs_shape` (metadata port: read=`fn(T)->?`, submit=`fn()->?`),
`concurrency_methods_resolve_via_harvested_table_with_subst` + `executor_submit_accepts_any_return_rejects_arity`,
`native_std_module_is_file_backed` (resolver — converted from `native_std_module_is_virtual`, no virtual
module remains), the VM 3-engine regression guard `concurrency_file_backed_three_engine`, and the sibling
provenance asserts retargeted to `std.ffi`'s residual type-license tail. Full suite green (3174 lib + all
integration), clippy clean, `grammar.bnf`/conformance unchanged. 3-engine CLI parity re-verified on
`examples/{shared,rwshared,atomic,executor,parallel_shared,native_qualified}.chz` (default==serial==expected).

**✅ NATIVE-PRELUDE TABLE — phase 1 (refactor-only, pure functions) (2026-07-01).** A single synthetic
Rust `const PRELUDE: &[PreludeFn]` in `src/checker/mod.rs` is now the **SINGLE SOURCE OF TRUTH** for the
four first-class universe FUNCTIONS (`print`/`ord`/`chr`/`panic`), replacing the scattered hard-coded
match arm each phase used to keep. Row shape: `PreludeFn { name, intrinsic: Intrinsic, first_class,
make_sig }` where `enum Intrinsic { Print, Builtin }` (`Print` ⇒ direct call lowers to
`CallPrint`/`CallPrintSep`; `Builtin` ⇒ `Op::CallBuiltin(name, argc)`). The signature is carried as a
const-safe `make_sig: fn() -> FnSig` fn pointer (a `FnSig` holds `Vec`/`Box`, so it can't be a literal
`const` field) and stays PRIVATE so the `pub(crate)` row never leaks the module-private `FnSig`.
Every phase now READS the table: checker `is_firstclass_builtin_fn` = table `.first_class`, `builtin_sig`
delegates the four sigs to `(make_sig)()`, the value-position `Ty::BuiltinFn` arm is unchanged (already
sources `builtin_sig`); compiler `is_builtin` + `compile_call`'s direct-call opcode selection derive from
`prelude_fn(name).intrinsic`; interp `builtins::is_builtin` derives the same way. **ZERO observable
behavior change** — direct calls emit byte-identical bytecode (`print(x)`→`CallPrint(1)`,
`print(x, sep=…)`→`CallPrintSep`, `ord`/`chr`/`panic`→`CallBuiltin`), the hot path only gains a
compile-time table lookup, and three-engine byte-identical parity (interp / `--serial` VM / M:N VM) on
`examples/defer_builtin_value.chz` + all existing guard tests stays green. **Native impls UNTOUCHED**:
`vm::do_builtin` arms, `builtin_ord`/`builtin_chr`, the print stringify, and all name-keyed runtime
dispatch (`Value::Builtin`/`Obj::Builtin`, `LoadBuiltin`, spawn/wire/snapshot) stay exactly where they
are — the table is COMPILE-TIME METADATA ONLY (the `NativeFn` host seam only takes int/str/map args,
which is precisely why `print` needs its dedicated value/opcode path). New drift guard
`prelude_table_is_single_source_of_truth` (checker/tests.rs) + a bytecode pin test
`direct_builtin_calls_lower_to_specialized_opcodes` (compiler tests) lock the invariant that every phase
agrees with the table — the whack-a-mole class this track kills.

**✅ NATIVE-PRELUDE TABLE — phase 2a (refactor-only, scalar-conversion ctors) (2026-07-02).** Added a
third intrinsic kind `Intrinsic::Ctor` and folded the **five scalar-conversion CONSTRUCTORS**
(`int`/`float`/`str`/`bytes`/`bytearray`) into the table as rows with `first_class: false` (ALWAYS —
types/ctors are NOT first-class values, uniform with `f := Point` / `f := List` staying rejected). Each
row carries the exact `FnSig` its old hard-coded `builtin_sig` arm did (`int`/`float`/`str` take `?`→
`Int`/`Float`/`Str`; `bytes`/`bytearray` take `?`→`Bytes`/`ByteArray`) and dispatches on a direct call
to the same name-keyed `Op::CallBuiltin(name, argc)` — so `int("5")`, `int("ff")`… emit **byte-identical
bytecode** and `vm::do_builtin`'s native conversion arms are **UNTOUCHED** (metadata only). The now-dead
`int`/`float`/`str`/`bytes`/`bytearray` arms in `builtin_sig` were deleted (the `prelude_fn` early-return
supplies them); `is_builtin` in compiler + interp drop those five from the hard-coded `matches!` and
read them from the table via `Intrinsic::Builtin | Intrinsic::Ctor`. **NON-FIRST-CLASS enforced**: every
first-class value path (`is_firstclass_builtin_fn`, `Ty::BuiltinFn` arm, `LoadBuiltin`) gates on
`.first_class == true`, so a `Ctor` row never leaks a first-class value — `f := int` / `defer str(...)`
stay rejected on the identical fall-through path as `f := List`, with zero new guard code. The drift
guard is extended (Ctor name-set, no `Ctor` row is first-class) plus a `scalar_ctor_conversions_parity`
two-engine test and the extended
bytecode pin. **The GENERIC / reserved-type container ctors** (`List`/`Map`/`Set`/`range`) were folded
in later → **phase 2b** (below), keeping their generic type-identity in `resolve_type`. **North-star:**
realized in **phase 3a** below — the signatures moved to a real `.chz` prelude; only `print` (variadic)
+ `range` (arity overload) remain synthetic carve-outs. (The earlier ".chz prelude blocked on user-facing
variadics" framing is **superseded**: a `native`-decl signature needs no `*args` syntax — only `print`'s
`sep=`/`end=` variadic still can't be spelled in `.chz`, so it stays the sole synthetic function row.)

**✅ NATIVE-PRELUDE — phase 4c-ffi (`std.ffi`'s 59 FUNCTION sigs made file-backed:
`std/ffi.chz`) (2026-07-02).** REFACTOR-ONLY, **ZERO observable change / three-engine byte-identical** —
the proven phase-4b/4d/4e/4f pattern applied to `std.ffi`. All **59** callable fns (`null`/`is_null`, the
`load_*` family — 14 loads × {base, `_at`} — the `store_*` family — 13 stores × {base, `_at`} — and
`alloc`/`alloc_zeroed`/`free`) are now bodyless `native fn` decls in a real **`std/ffi.chz`**, harvested by
the checker via `harvest_native_module`; the resolver loads the file while **KEEPING the `native` marker**
(runtime dispatch stays name-keyed via `native_members("std.ffi") => ffi::MEMBERS` — bytecode + `src/native/ffi.rs`
UNCHANGED). `std.ffi` added to the shared `crate::native::is_file_backed_native` predicate. The migration is
**PARTIAL BY DESIGN**: a `native fn` produces a `sig.functions` entry, but `std.ffi` ALSO exports **type-license-only**
names — the opaque `ptr` handle + the eight fixed-width C-ABI integer names (`int8..uint64` in `ffi::TYPE_NAMES`)
— which resolve to `Ty::Ptr`/`Ty::Int` via `resolve_type` gated on `imported_ffi_types` and have NO `.chz`
decl syntax (no way to spell a bare type-license name aliasing a builtin scalar). So the `native_module_sig("std.ffi")`
arm is **REDUCED to only that type-license tail** (the `TYPE_NAMES` loop + the `ptr` insert), mirroring the
residual `std.net`/`std.concurrency`/`std.time` arms — full deletion is NOT achievable without inventing a new
decl kind (out of scope). **Non-obvious blocker solved:** harvesting `native fn null() -> ptr` resolves `ptr`
through `resolve_type`'s `ptr` arm, which requires `imported_ffi_types.contains("ptr")`, but harvest runs WITHOUT
`begin_module` (that set is empty) → `harvest_native_module` now **transiently licenses** every `sig.types` name
that is `ptr`/in `TYPE_NAMES` into `imported_ffi_types` before PASS 2 and **restores exactly those** after (the
direct analog of the existing `struct_names` transient; driven off `sig.types` so module-agnostic; no leak — a
sibling that never imported std.ffi still rejects bare `ptr`). Every store (26) + `free` spells an explicit
`-> nil` (harvest maps a MISSING ret to `Ty::Unknown`, NOT `Ty::Nil` — the old arm returned `Ty::Nil`, so the
explicit `-> nil` is correctness-critical to byte-match). Tests: `enc_crypto_uuid_time_sig_from_file_not_native_module_sig`
(inverted for ffi — arm's fns gone, `ptr`+`TYPE_NAMES` license kept), `ffi_fn_sigs_exact` (all 59 harvested sigs
byte-equal to the deleted for-loops + MEMBERS len==59 cross-check), `ffi_ptr_license_does_not_leak_past_harvest`
(per-name `import ptr`/`int32` license + no cross-module leak), the existing 10 `ffi_*` typecheck tests unchanged,
and the 3-engine golden `golden_std_native_4c_chz_matches_expected_and_interp` (`examples/std_native_4c.chz`
alloc/store/load round-trip, VM==interp==M:N — FFI is layout-dependent UB, so a real round-trip, not goldens alone).
`grammar.bnf` unchanged (conformance green). **Remaining `native_module_sig` content after 4c-ffi:** `net`
(methoded `Socket`/`Listener`) + `concurrency` (opcode type-licensing) + `ffi`'s type-license tail — `net`
migrated next (see phase 4c-net below).

**✅ NATIVE-PRELUDE — phase 4c-net (native METHOD-binding capability built + `std.net` made file-backed:
native TYPEs `Socket`/`Listener` WITH method tables + `connect`/`listen` declared in `std/net.chz`)
(2026-07-02).** A genuine checker CAPABILITY build (not a mechanical batch): a `native fn` inside a
`native struct` body is now a body-less **method** sig, harvested into that type's method table
(`harvest_native_module` PASS 1b) and checked via the **normal method-resolution path** — retiring the
bespoke `socket_method_sig`/`listener_method_sig` arms. `std.net` becomes a **real `.chz`**: `Socket`
(`read`/`write`/`close`) + `Listener` (`accept`/`addr`/`close`) native structs + `connect`/`listen`
free fns, all harvested. **CRITICAL additive subtlety:** `Socket`/`Listener` KEEP resolving to the
RESERVED `Ty::Socket`/`Ty::Listener` (opaque VM handles — NOT a fresh `Ty::Struct`), so VM interception
(`connect`/`listen`/`read`/`write`/`accept` stay VM-intercepted by name) + `connect`'s `Result[Socket]`
return are UNCHANGED. The harvested method table is re-seeded (method-table only, NO bare licensing —
`net_socket_seed`/`net_listener_seed` → `seed_stdlib_structs`, the `ref_seed` precedent) into
`self.structs["Socket"]`/`["Listener"]`, and the `Ty::Socket`/`Ty::Listener` method arms look it up
there. Bare-name annotation stays import-gated via `imported_net` + `resolve_type`'s reserved arm.
**Gotcha fixed:** the native-module harvest branch never ran `begin_module`, so `current_module_is_stdlib`
was stale-false → `resolve_type(Socket)` in `connect`'s return would error `unknown type 'Socket'`; set
`c.current_module_is_stdlib=true` at the top of the native branch (every native module IS std;
additive-safe). This RETIRES the hand-built `"std.net"` `native_module_sig` arm (default-empty now).
`attach_native_module_metadata` port = **no-op for net** (no Socket/Listener method recovers a return
type from a closure arg — all concrete plain/optional-tail). Runtime (VM/interp socket/listener dispatch,
connect/listen interception, `bind_import` Socket/Listener skips) **UNTOUCHED**. **Three-engine
byte-identical** (checker-only cut): `net_sig_from_file_not_native_module_sig` (provenance — arm gone,
harvested Socket/Listener method sigs byte-exact to the retired bespoke arm), the D6c
`socket_read/write/listener_accept_with_timeout_type_checks` + arity/type rejects (now resolve via the
harvested table), `native_struct_parses_native_methods` (parser), `net_from_import_runs_both_engines`
(extended: whole-module + from-import, method calls in a checked body — VM==interp), existing
`examples/socket_timeout.chz` (--parallel golden) + `echo_server.chz`/`echo_server_spawn.chz` unchanged.
`grammar.bnf` unchanged (native-decl grammar exists from 3a/4a; conformance green). **Roadmap (DONE):**
after 4c-ffi + 4c-net + **4c-concurrency** (the last migration — generic types `Shared`/`RwShared`/
`Atomic`/`Executor`, see the top block), `native_module_sig` retains only **`ffi`'s residual type-license
tail** (`ptr` + fixed-width `int8..uint64`) + **`time`'s `timer`** opcode-license — no runtime member.

**✅ NATIVE-PRELUDE — phase 4f (`std.process` + `std.request` made file-backed: native TYPE + FNs
declared in `std/process.chz` / `std/request.chz`) (2026-07-02).** Mechanical application of the proven
phase-4b regex pattern to the two remaining fields-only native-struct modules. `std.process` and
`std.request` are no longer *file-less virtual* modules — each is now a **real `.chz`** whose fields-only
`native struct` (`ProcResult` [stdout, stderr, code] / `Response` [status, body, headers]) + `native fn`s
(process: `cmd`/`run`/`run_args`; request: `get`/`post`/`request`/`put`/`patch`/`delete`/`head`) are declared
**in-module** and harvested by the checker via `harvest_native_module`. The resolver loads the real files
while **keeping the `native` marker** (runtime member dispatch stays name-keyed via `native_members`;
bytecode UNCHANGED). This RETIRES BOTH the hand-built `"std.process"`/`"std.request"` **fn arms** AND their
`export_struct` **type arms** in `native_module_sig` (which now returns default-empty for both), plus the
post-match optional-tail install block. The **one subtlety over regex** — request's `get`/`post`/`request`
carry an OPTIONAL trailing `timeout_ms` — is spelled as a **trailing `= 0` default** in the `.chz`; harvest
PASS 2 counts trailing `default.is_some()` params and lowers to `FnSig::optional_tail` (min_params = len-1),
byte-identical to the deleted hand-built install. To admit that spelling, `parse_native` now calls
`parse_params(true)` (the grammar already permitted a param default in `<nativeDecl>`; the parser was merely
stricter — flipping it brings the parser INTO conformance, no `grammar.bnf` edit, conformance green). The
default EXPR is a **marker only** — desugar's `collect_module_reg` ignores `StmtKind::Native`, so it is never
injected at a call site (`arg_count()` stays truthful). `native fn`/`native struct` in a USER file is still a
clear checker error (stdlib-only hoist rejection fires before any default). The remaining hand-built runtime
layout copies (compiler `Compiler::new`, interp finalize, `native/process.rs`+`native/request.rs`,
`seed_stdlib_structs`) stay, **field-order drift-guarded** by `procresult_chz_matches_handbuilt_layouts` +
`response_chz_matches_handbuilt_layouts`. Import-gating (`ProcResult`/`Response` bare names licensed only by
importing their module) + the both-engine pure-type `bind_import` skip preserved by construction (harvest
forces origin=Builtin). **ZERO observable change / three-engine byte-identical** (checker/resolver-only cut):
`process_fn_sigs_exact` + `request_fn_sigs_exact` (sigs + StructInfo now come from the files, byte-equal to the
deleted arms; request's optional-tail min_params exact), `regex_sig_from_file_not_native_module_sig` (inverted
— asserts both arms gone), `request_optional_timeout_arg_typechecks` (both arities check), `native_fn_allows_optional_trailing_default`
(parser), `process_request_file_backed_three_engine_parity` + `pure_type_import_no_fault_both_engines`
(VM==interp==M:N), existing `examples/process_polish.chz` + `sys.chz` goldens unchanged on both engines.
**Roadmap (DONE):** after 4b/4f/4c-ffi/4c-net + **4c-concurrency** (the last migration), `native_module_sig`
retains only **`ffi`'s residual type-license tail** (`ptr` + fixed-width `int8..uint64`) + **`time`'s
`timer`** opcode-license. `grammar.bnf` unchanged (native-decl grammar + param defaults exist from 3a/4a;
conformance green).

**✅ NATIVE-PRELUDE — phase 4e (4 pure-function native modules made file-backed:
`std.encoding`/`std.crypto`/`std.uuid`/`std.time`) (2026-07-02).** REFACTOR-ONLY, **ZERO observable
behavior change / three-engine byte-identical** — a mechanical replay of the proven phase-4b regex
pattern onto four **pure-function** modules (no methoded types). Each now ships a **real
`std/<M>.chz`** whose current members are declared in-module as bodyless `native fn`s
(encoding: the 8 str↔str/`Result[str]` codecs + `query_encode(params: Map[str,str]) -> str`;
crypto: `sha256`/`md5`; uuid: `v4()`/`uuid_seed(n) -> nil`; time: `now`/`monotonic`/`sleep_ms(ms) ->
nil`/`format`). The resolver loads the real file (`visit_native_file`, fallible like the prelude) while
KEEPING the `native` marker, so **all runtime member dispatch stays name-keyed via
`native_members("std.M")` — bytecode + dispatch UNCHANGED**; the checker harvests each file's `native fn`
sigs via the existing `harvest_native_module`. This **RETIRED** the hand-built `std.encoding`/`std.crypto`/
`std.uuid` arms in `native_module_sig` (deleted — default-empty now) and reduced the `std.time` arm to
its **one load-bearing line**: `sig.types.insert("timer")`. `timer` is the sole subtlety — an
**opcode-backed builtin** (NOT a callable native member: no runtime value, lowers via the compiler's
name→opcode dispatch), so it is DELIBERATELY *not* declared as a `native fn` (that would bind a
nonexistent runtime value and fault); its import-license (`import timer from std.time` / `import std.time`
+ bare `timer(ms)`) is preserved by that minimal arm, harvest then filling the 4 real time fns on top.
The two file-backed gates (resolver `visit_native_file` + checker harvest) now share one predicate
**`crate::native::is_file_backed_native(name)`** ({regex,encoding,crypto,uuid,time}) so the file-source
and AST-source stay provably in lockstep. Import-gating preserved; none of the 4 are in `MODULE_FN_DOCS`
(`module_fn_docs_all_resolve` unaffected). Tests: `enc_crypto_uuid_time_sig_from_file_not_native_module_sig`
(provenance — arms gone, timer license kept), `enc_fn_sigs_exact`/`crypto_fn_sigs_exact`/`uuid_fn_sigs_exact`/
`time_fn_sigs_exact` (sigs byte-equal to the deleted arms; `-> nil` fidelity for sleep_ms/uuid_seed +
`Map[str,str]` for query_encode), `import_timer_from_std_time_still_licensed_both_forms`,
`golden_timer_selective_import_three_engine` (VM==interp==M:N), `phase4e_user_file_native_fn_still_rejected`,
existing goldens (`golden_encoding_crypto_via_run_file`/`golden_uuid_via_run_file`/`golden_timer_chz_matches_expected_and_interp`)
unchanged. `grammar.bnf` unchanged (native-decl grammar exists from 3a; conformance green).

**✅ NATIVE-PRELUDE — phase 4d (five pure-function native modules made file-backed: `std.math` /
`std.io` / `std.os` / `std.rand` / `std.fs`) (2026-07-02).** REFACTOR-ONLY (no new capability — the
proven phase-4b regex pattern applied to pure-function modules): each of the five is now a **real
`std/<M>.chz`** whose members are bodyless `native fn` decls reproducing the EXACT prior sig, instead of
a *file-less virtual* module with a hand-built `native_module_sig` arm. The resolver's import loop loads
each real file (via the new shared authority **`crate::native::is_file_backed_native`** — now covering
`{regex, math, io, os, rand, fs}` — swapped in for the `name == "std.regex"` special-case at
`resolver/mod.rs`), **KEEPING the `native` marker** so runtime member dispatch stays name-keyed via
`native_members("std.M")` — **bytecode + dispatch UNCHANGED**. The checker graph loop harvests any
`is_file_backed_native` module via the existing `harvest_native_module`, then runs the new
**`attach_native_module_metadata(name, &mut sig)`** on EVERY native module to re-attach the three pieces
a `native fn` decl can't express: (a) hover docs (`MODULE_FN_DOCS`, moved out of the deleted arm tail),
(b) module CONSTANT values `math.pi`/`e` (enumerated from `native::native_consts`, no hardcode), and
(c) numeric-poly fns `math.abs` (int→int/float→float) via the new `MODULE_NUMERIC_POLY` side-table
(parallel to `MODULE_FN_DOCS`). The five `native_module_sig` arms are **DELETED** (the fn returns
default-empty for them). **ZERO observable change / three-engine byte-identical** (checker/resolver-only
cut): `math_io_os_rand_fs_sig_from_file_not_native_module_sig` (arms gone),
`math_io_os_rand_fs_representative_sigs_exact` (fn sigs + pi/e values + abs poly byte-equal to the deleted
arms), `math_io_os_fn_hover_doc_preserved`, `math_io_os_rand_fs_runtime_tables_unchanged` (dispatch
tables + `native_consts` untouched), `math_is_file_backed_native` (resolver), and the 3-engine golden
`golden_std_native_4d_chz_matches_expected_and_interp` (`examples/std_native_4d.chz`, VM==interp==M:N).
`module_fn_docs_all_resolve` now builds the effective sig via the graph (the migrated fns are harvested).
`native fn` in a user file still rejected; `grammar.bnf` unchanged (conformance green). **Remaining
`native_module_sig` content after 4d/4e/4f/4c-ffi/4c-net + 4c-concurrency (the last migration):** only
`ffi`'s residual type-license tail (`ptr` + fixed-width `int8..uint64`) + `time`'s `timer` opcode-license.
(`net` migrated in 4c-net, `ffi` fns in 4c-ffi, `concurrency`'s four generic types in 4c-concurrency.)

**✅ NATIVE-PRELUDE — phase 4b (regex module made file-backed: native TYPE + FNs declared in
`std/regex.chz`) (2026-07-02).** NEW CAPABILITY (import-gated native **module members**): `std.regex` is
no longer a *file-less virtual* module — it is now a **real `std/regex.chz`** whose `native struct Match`
+ five `native fn`s (`is_match`/`find`/`find_all`/`replace_all`/`split`) are declared **in-module**,
exactly how `Ref` lives in `std/ref.chz`. The resolver's import loop loads that real file (fallible, like
the always-linked prelude) instead of `visit_native` injecting an empty AST, but **KEEPS the `native`
marker** (`native: Some("std.regex")`) so all runtime member dispatch stays name-keyed via
`native_members("std.regex")` — bytecode + dispatch **UNCHANGED**. The checker's native-module arm now
calls the new **`Checker::harvest_native_module`** (replacing `harvest_native_struct_stub`), which harvests
BOTH the `native struct` (→ `struct_defs`/`types`, `origin=Builtin` forced) AND the `native fn` sigs (→
`sig.functions`, the import-gated module-member surface) from the parsed in-module decls; a two-pass harvest
(transient `struct_names` insert during pass-2 so a fn return like `Result[Option[Match]]` resolves `Match`,
removed after → import-gating preserved). This **RETIRED** both the phase-4a companion stub
(`std/regex.stub.chz` + `harvest_native_struct_stub`, deleted) AND the hand-built `"std.regex" =>` arm in
`native_module_sig` (deleted — it returns default-empty for regex now). Match stays **import-gated**
(bare name licensed only by `import std.regex` / `import Match from std.regex`; `regex.Match(...)`
qualified); the 4 remaining hand-built runtime layout copies (`seed_stdlib_structs`, `Compiler::new`, interp
finalize, `native/regex.rs`) stay, **field-order drift-guarded** by `regex_chz_match_matches_handbuilt_layouts`.
**ZERO observable change / three-engine byte-identical** (checker/resolver-only cut): `regex_fn_sigs_exact`
(the 5 FnSigs + Match StructInfo now come from the file, byte-equal to the deleted arm),
`regex_sig_from_file_not_native_module_sig` (asserts the arm is gone), `std_regex_is_file_backed_with_native_marker`
(resolver), `regex_match_file_backed_three_engine_parity` (produce/field-read/`import Match from`/qualified,
VM==interp==M:N — locks the pure-type `bind_import` skip), existing regex goldens
(`golden_regex_demo_via_run_file`) unchanged. `grammar.bnf` unchanged (native-decl grammar exists from 3a/4a;
conformance green). **Roadmap:** `Response`/`ProcResult` are now DONE too (phase 4f — see the entry above).
net `Socket`/`Listener` are DONE too (phase 4c-net — the first methoded native types, native METHOD
binding built there). Remaining phase-4c = concurrency (`Shared`/`RwShared`/`Atomic`/`Executor`)
file-backed with `native struct` + method binding; of Tier-3, `Option`/`Result`'s variant SHAPE is now
file-backed too (phase 5b — a drift-guarded MIRROR; the `?`/match/construction WIRING stays Rust-wired,
see the entry above), and `Iterator` (a protocol + reserved value type, not an enum) stays native
(deferred to phase 5c).

**✅ NATIVE-PRELUDE — phase 4a (`native struct` syntax + companion-stub loader for file-less native
modules) (2026-07-02) — companion stub RETIRED in phase 4b (above).** NEW LANGUAGE FEATURE (the **type-level** analog of phase-3a `native fn`/`native
ctor`): `native struct Name:` with an indented block of **body-less field decls** declares a native
(Rust-backed) type's **checker signature** (field layout + type params) in Chezzi; the runtime layout +
method dispatch stay **native** (name-keyed). **Fields-only** for this cut (a `fn`/`test` method sig or a
field `= default` in the body is a parse error; bodyless native **method** sigs are phase-4b), **PRELUDE/
STD-ONLY** (a `native struct` in a user `.chz` is a clear checker error — *native struct declarations are
only allowed in standard-library modules*), TOP-LEVEL-only (parser; nesting reuses the existing depth>1
`Token::Native` guard). **COMPANION-STUB LOADER** (the general mechanism for **file-less** native modules):
`std.regex` is a *virtual* module — `resolver::visit_native` injects an empty AST, there is no
`std/regex.chz`. Its `Match` type's signature now lives in a **parse-only companion stub
`std/regex.stub.chz`** (embedded via `include_str!`), which is **never** added to the runnable module graph
(not always-linked, not executed) — `Checker::harvest_native_struct_stub` parses it solely to harvest its
`native struct` decls into `std.regex`'s `ModuleSig` (`struct_defs` + `types`), **replacing** the deleted
hand-built `"std.regex" => export_struct("Match", …)` arm in `native_module_sig` (the regex FUNCTIONS
`is_match`/`find`/`find_all`/`replace_all`/`split` STAY hand-built there; only `Match`'s StructInfo moved).
The harvest **forces `origin=StructOrigin::Builtin`** (load-bearing: drives `imported_builtin_types` on
import → both engines' name-keyed pure-type `bind_import` skip stays correct). Match stays **import-gated**
(bare name licensed only by `import std.regex` / `import Match from std.regex`; `regex.Match(...)` qualified)
— reuses the existing native-types additive pattern; **runtime layout + bytecode UNCHANGED** (the 5 hand-built
layout copies — `seed_stdlib_structs`, `Compiler::new`, interp finalize, `native/regex.rs` — stay, drift-
guarded by `match_stub_matches_handbuilt_layouts`). **ZERO observable change / three-
engine byte-identical** (checker/parser/grammar-only cut): new `regex_match_stub_migration_three_engine_parity`
(produce/field-read/`import Match from`/qualified, VM==interp==M:N), provenance + drift-guard + user-file-
rejected checker tests, parser tests, `grammar.bnf` gains `<nativeStructDecl>` + accept-corpus
`native_struct.chz` (conformance green). **Roadmap:** phase-4b = bodyless-**method**-sig→native binding
(analogous to native fn's proven bodyless-sig binding) + migrate the remaining Tier-2 native types
(`Shared`/`RwShared`/`Atomic`/`Executor`, `Response`/`ProcResult` + the rest of regex) fully out of
`native_module_sig`, and unify the remaining hand-built `Match` layout copies onto the stub. `native enum`
if ever needed. **Tier-1** (`Ref`) already done; **Tier-3** (`Option`/`Result`/`Iterator`) INTENTIONALLY
stays native (documented carve-out).

**✅ NATIVE-PRELUDE — phase 3a (`native fn`/`native ctor` syntax + always-linked `std/prelude.chz`)
(2026-07-02).** NEW LANGUAGE FEATURE (the north-star for FUNCTIONS made concrete): the internal analog of
`extern "lib":` (FFI). `native fn NAME(params) -> ret` declares a **first-class** universe-function
intrinsic (⇒ `Intrinsic::Builtin`, `first_class=true`); `native ctor NAME(params) -> ret` a
**non-first-class** scalar/type constructor intrinsic (⇒ `Intrinsic::Ctor`, `first_class=false`). Bodyless
(like an `extern` sig, NEWLINE-terminated), **PRELUDE/STD-ONLY** (a `native` decl in a user `.chz` is a
clear checker error — a user can't bind a name to a nonexistent intrinsic), TOP-LEVEL-only (parser). The
**eight** universe builtins (`ord`/`chr`/`panic` fns; `int`/`float`/`str`/`bytes`/`bytearray` ctors) now
declare their SIGNATURES in a real **`std/prelude.chz`** that the resolver **always-links** into every
graph (same seam as `std/ref.chz`, injected before the entry DFS so the entry stays LAST; deduped). The
signatures moved OUT of the Rust `make_sig`/`sig_*` fns into the parsed decls (harvested into the checker's
`native_prelude_sigs`, read by `Checker::builtin_sig`); the **hollow** Rust `PRELUDE` table keeps only
name→intrinsic→first_class METADATA (the backends `compiler::is_builtin`/`interp::builtins::is_builtin`
have no graph access and read only that). `print` stays the **one** synthetic function row (variadic).
**DYNAMIC-PARAM CONVENTION** (native-decl-scoped — introduces NO user-facing `any`/`never`): an
UNANNOTATED param = the dynamic "accepts anything" type (`Ty::Unknown`); a decl with NO `-> ret` =
native-controlled/never (`Ty::Unknown` return — how `panic` is spelled). **Backends UNCHANGED**: a `native`
decl compiles to NO bytecode / NO binding (skipped like `StmtKind::Extern` in compiler + interp; never a
callable user fn); direct calls to the eight names emit byte-identical `CallBuiltin`/`CallPrint` and
`vm::do_builtin` dispatch stays name-keyed. **ZERO observable change** — the drift guard
`prelude_table_is_single_source_of_truth` is extended to cross-check the parsed `.chz` decl set/kinds vs
the hollow table AND each parsed `FnSig` vs its historical shape; new `native_prelude.chz` three-engine
golden + parser/checker/resolver tests. `grammar.bnf` gains `<nativeDecl>` (conformance green).
**Roadmap (native-in-Chezzi track):** phase 2b (**DONE** — see below) folded the generic container
ctors' (`range`/`List`/`Map`/`Set`) DISPATCH into the table (type-identity stays in `resolve_type`).
**Phase 4a** (**DONE**) = `native struct` syntax + the companion-stub
loader, with `regex.Match` migrated (fields-only). **Phase 4b** (**DONE** — see the phase-4b entry above) =
`std.regex` made **file-backed** (`std/regex.chz`), native TYPE + FNs declared in-module, companion stub +
`native_module_sig` regex arm RETIRED. **Phase 4f** (**DONE** — see the phase-4f entry above) =
`std.process` + `std.request` made file-backed (`ProcResult`/`Response` + their fns), both `native_module_sig`
fn arms AND `export_struct` arms RETIRED; request's optional `timeout_ms` spelled as a trailing `= 0` default.
**Phase 4c** = bodyless native **method**-sig→native binding + migrate the remaining Tier-2 native
(Rust-backed) TYPES (`Shared`/`RwShared`/`Atomic`/`Executor`, net `Socket`/`Listener`) fully out of the
`native_module_sig` hand-tables (bodies still native), plus `native enum` if needed. **Tier-1** (the `Ref` struct-modeled type) is already done
(always-linked `std/ref.chz`). **Tier-3** (`Option`/`Result`/`Iterator`) INTENTIONALLY stays native —
too deeply coupled to `match`/`?`/generator desugar to express as a plain `.chz` decl; this is a
documented, deliberate carve-out, not a gap.

**✅ `modules.last() == entry` invariant hardened against always-injected prelude stubs (2026-07-02).**
The resolver `build_graph_with_entry_source` always-injects `std/prelude.chz` then `std/ref.chz` BEFORE
the entry DFS, so if the ENTRY file itself IS one of those stubs (`chezzi run std/prelude.chz`) its own
visit is deduped by `visited` and the graph would end mid-list — `graph.entry != modules.last()` — and
the positional-entry consumers (compiler `entry_idx = modules.len()-1`, both engines' `entry_home() =
modules.last()`) would designate the WRONG module as entry (for test-fn discovery / manifest `:function`
invocation). A localized guarded stable reorder in the resolver (right after `ModuleGraph` construction,
before `desugar::run`) now moves the `graph.entry` module to the tail iff it isn't already last — a
strict no-op for the normal case (entry is a user file, already last → zero behavior change), and stable
for all other modules so deps still precede dependents. This **removes the phase-3a latent-contract
follow-up** and **unblocks stacking more always-linked modules safely** in phase 4. Covered by resolver
tests (`entry_is_prelude_stub_still_designated_last` + ref forward-guard) and a three-engine run-clean
parity test (`entry_is_always_linked_stub_runs_clean_three_engine`: cooperative VM / `--parallel` /
interp all Ok, empty stdout, byte-identical). Behavior-preserving; three-engine parity.

**✅ NATIVE-PRELUDE TABLE — phase 2b (refactor-only, generic container ctors) (2026-07-02).** Folded the
**four GENERIC / reserved-type container CONSTRUCTORS** (`range`/`List`/`Map`/`Set`) into the `PRELUDE`
table as `Intrinsic::Ctor` rows with `first_class: false` — a mechanical mirror of phase 2a applied to
the last synthetic-table carve-outs, completing the goal that **every universe builtin's `CallBuiltin`
DISPATCH + name-set flows through the one table**. `compiler::is_builtin` + `interp::builtins::is_builtin`
drop the hard-coded `matches!(name, "range"|"Set"|"List"|"Map")` and become **pure table reads** (the
`prelude_fn` direct-call arm now emits their `CallBuiltin`, byte-identical to the old hard-coded arm —
type-args are type-erased before the compiler, so `List[int]()` == `List()` at the opcode level). Unlike
the scalars, these are **generic / carry reserved-type identity**, so — as the task required — they are
**table-sourced for DISPATCH ONLY, deliberately NOT `.chz`-declared** (native ctor generic-decl support
is a later, maybe-never concern): their generic **TYPE-IDENTITY** (`List[int]` → `Ty::List(Int)`, the Map
hashable-key check, range arity/overload) is **NOT a flat `FnSig`** and stays in
`resolve_type`/`infer_named_call`, with `builtin_container_sig` supplying only a flat display/placeholder
sig. Cross-link comments pin the split (table = dispatch, `resolve_type` = generic identity) and the drift
guard `prelude_table_is_single_source_of_truth` now **asserts it can't rot**: the table surface MINUS the
four container ctors equals the eight `.chz` decls + `print`, and each container ctor is a non-first-class
`Ctor` row that is NOT in the parsed `.chz` decl set. **ZERO observable change** — `range(5)`,
`range(1,10,2)`, `List()`/`List[int]()`, `Map()`, `Set([1,1,2])`, generic inference (`xs := List[str]()`),
reserved-type errors (a user `struct List` still rejected), value-position rejection (`f := List`/`f :=
range` still checker errors), and `range[int]()` still errors (Ctor membership is orthogonal to
`name_is_generic`) — all identical, identical bytecode. New `container_ctor_parity` two-engine test +
`container_ctor_not_firstclass_value` checker test + extended bytecode pin; `vm::do_builtin`
`builtin_range/list/map/set` dispatch **UNTOUCHED** (name-keyed). Container ctors are now table-sourced for
dispatch **though not `.chz`-declared** (generics).

**✅ `Ref` promoted to a RESERVED GLOBAL backing the `ref` keyword — import-free (2026-07-01).** The
`ref T` binding modifier and the explicit `Ref[T]` box now work with **no `import std.ref`**. `Ref`
joins `Result`/`Option`/`Iterator`/`Channel` in the reserved-global class (`is_reserved_type`) — the
sanctioned set that backs core syntax — so a user `struct Ref` is always rejected as reserved. Mechanism
(minimal, three seams, NOT a native rewrite — the `.chz` stays the single source): (1) the resolver
**always-links `std/ref.chz`** into every program's module graph (injected as `order[0]` before the entry
DFS in `build_graph_with_entry_source`, deduped if already imported; entry stays LAST so
`modules.last()==entry` holds); (2) the checker **caches std.ref's real `StructInfo`** (layout +
`get`/`set`/`update` from the checked module — `ref_seed`) and **re-seeds it bare** in every module's
`seed_stdlib_structs` (import-free `struct_names`/`bare_types`), with `is_reserved_type += "Ref"` and a
`current_module_is_stdlib` exemption so std.ref's own `struct Ref[T]` decl stays legal; (3) the compiler
and interpreter each expose `Ref` **bare in every module's `bare_types`** (guarded on the struct being
registered) so the ctor lowers import-free on all engines. `import std.ref` is now a **harmless no-op**
kept for compatibility (idempotent `bind_import` inserts — no dup/shadow error). Three-engine
byte-identical parity via new golden `examples/ref_no_import.chz` (ref keyword + explicit `Ref[int]` +
closure-capture aliasing; `run_file` == `interp` == `run_file_parallel`); checker tests
`ref_keyword_and_type_work_without_import` / `import_std_ref_is_harmless_noop` /
`user_struct_named_ref_now_reserved`. `ref T` semantics (Rc<RefCell> box, persists through closure
capture) unchanged — only the import requirement removed. Docs: `docs/syntax.md`/`docs/stdlib.md`.

**✅ Swift-style KEYWORD ARGUMENTS through a function VALUE (2026-07-01).** Named arguments now work
through a first-class **function value**, not just a direct call: `g := greet; g(name="Bob",
greeting="Hi")` prints `Hi Bob`, keywords may be reordered, and a `fn(name: str)->nil` **HOF parameter**
accepts `f(name="X")`. `Ty::Func`/`Type::Func` gained a `labels` field (parallel to `params`) — built
from a user fn's / closure's param names and from an annotation's optional `IDENT:` labels (parser
`parse_fn_type_param`). Labels are **SURFACE-ONLY** (Swift SE-0111): a new equality-neutral `FnLabels`
wrapper makes the derived `Ty` `PartialEq` ignore them, and `compatible`/`assignable`/`unify`/`Display`/
`sendable` all `..`-ignore them, so `fn(str)->nil` ≡ `fn(name:str)->nil` — **zero** regression to
HOF/callback/protocol/subtyping and no Display/snapshot churn. Resolution is a checker-recorded
**side table** (`KeywordTable = HashMap<KeywordKey, Vec<usize>>`, `KeywordKey = (module idx, fragment-ctx span, fragment ordinal, first-named-arg span)`) mirroring the `extern_sigs`
precedent EXACTLY: `resolve_keyword_calls{,_standalone}` run the same deps-first pass and harvest a slot
**permutation** over the combined `[positional ++ named]` arg list, populated in BOTH the single-module
(`ok`/`check_src`) and multi-module (`check_graph`) paths; both backends read it in `compile_call` /
`eval_call` to lower a value+keyword call to a **plain positional `Op::Call`** — the runtime ABI stays
positional and UNCHANGED (`src/vm` untouched — the `DeferCall`/`SpawnCall` lowerings consult the same
table so `defer d(name=…)` / `spawn s(name=…)` reorder too, no check-passes-then-traps hole). **SCOPE-CUT** (SE-0111): a value call must supply every
parameter — declaration-site **defaults do NOT fill through a value** (`h := hasdefault; h()` errors,
direct `hasdefault()` still fills); a first-class **built-in** value takes no keywords. Direct-call
keyword resolution (desugar), struct ctor/method named+default args, and `print` `sep=`/`end=` are all
UNCHANGED — desugar just stops rejecting value+keyword calls (Ident/expr callee) and defers them to the
checker. Positional value calls are untouched (the table is read only when `named` is non-empty → no
hot-path cost, `benches/run.chz` unchanged). Three-engine byte-identical parity
(`examples/keyword_value.chz` + a cross-module `keyword_value_xmod/`); grammar/`docs` updated
(`<fnParam>` optional label, conformance green).
  - **Fix (post-review):** two soundness holes in the above. (1) **Chained value keyword calls**
    (currying: `g(a=…)(b=…)` where a value returns another value) aliased one `KeywordTable` slot —
    the parser gives every link of a postfix chain the SAME call-node span (`parse_postfix`'s
    `let span = e.span;`), so the later permutation overwrote the earlier and the compiler/interp
    applied the wrong perm (out-of-range index → panic, or silent mis-route). The table is now keyed
    by a per-call-unique span (`checker::keyword_key_span` = the first named-arg VALUE expr's span,
    always present when recording), computed identically at the record site and all six backend
    lookups. (2) The **spawn airlock** sendability gate iterated only positional `args`, so a
    non-sendable value passed by LABEL to a spawned function value (`spawn h(f=cb)`, `spawn h(r=box)`)
    crossed unchecked while the positional form was rejected — the gate now chains `named` too.
    Regression tests: `golden_keyword_value_chz*` (chained curry line), `spawn_non_sendable_keyword_arg_rejected`,
    `spawn_non_sendable_ref_keyword_arg_rejected`.
  - **Fix (post-review #2):** the first-named-arg span above is unique only *within one lexed source*.
    Every `{…}` **string-interpolation** fragment is re-lexed from a fresh source, so its sub-expression
    spans restart at `(1,1)`; two value+keyword calls in different fragments whose first named-arg value
    lands at the same fragment-relative column (`"{a(y=1, x=10)} {b(p=3, q=2)}"`) collided on one
    `KeywordTable` slot and the earlier call was lowered with the WRONG permutation on all three engines.
    The key gained two **fragment discriminators** — the whole-string span + the fragment's 0-based
    ordinal — maintained identically by the checker (`check_interpolation`), compiler (`compile_str`),
    and interp (`interpolate`) at the interpolation boundary (inert defaults outside interpolation, so
    non-interpolation keying and the positional hot path are unchanged). `examples/keyword_value.chz`
    grew a colliding-offset interpolation line; regression test
    `keyword_value_interpolation_fragments_do_not_alias`.
  - **KNOWN LIMITATION (interp-only, accepted 2026-07-01):** the interp's `kw_frag_ctx`/`kw_frag_ord`
    are live mutable state set per interpolation fragment; they **leak** into callee bodies invoked from
    a `{…}` fragment and across a `recover:`-caught fault (save/restore only on the Ok path), so a
    value+keyword call reached that way is looked up under the wrong key → interp mis-resolves while the
    VM (static, resolves at compile time) is correct. **The user-facing engines (default M:N + `--serial`,
    both VM) are correct**; the divergence is only against the **deprecated interp parity oracle**, in the
    narrow `recover:`+interpolation+value-keyword combo, and no current golden test exercises it. Accepted
    rather than fixed because **interp is slated for removal** (decision: don't harden a dying engine).
    **When interp is deleted, also strip the frag-context machinery** (`kw_frag_ctx`/`kw_frag_ord` in
    `checker`/`compiler`/`interp`, ~47 refs) — the `KeywordTable` key simplifies to
    `(module, first-named-arg span)` since fragment discriminators only existed for the interp lookup.

**✅ First-class universe builtin FUNCTIONS `print`/`ord`/`chr`/`panic` (2026-07-01).** These four
universe functions are now **first-class values**: `defer print("World")` works as a bare call (the old
gate error *"built-ins and constructors must be wrapped in a function"* is gone for these names), and
they can be bound / passed like any function (`f := ord; f("a")`, HOF arg). Scope is **exactly** those
four — `len` stays method-only (`xs.len()`), and **type / container / runtime constructors** (`int`,
`str`, `List`, `Map`, `Channel`, `range`, …) plus user struct/enum ctors remain **non-first-class**
(still wrapped, uniform with `f := Point`). A new dedicated runtime value variant carries them:
`Obj::Builtin(Box<str>)` (VM) / `Value::Builtin(Rc<str>)` (interp) — pure-code, **SENDABLE** (crosses
the spawn airlock by cloning the name: cooperative VM via `SnapValue::Builtin`, M:N OS-thread engine via
the by-value `WireValue::Builtin`). Checker: `is_firstclass_builtin_fn` whitelist relaxes the `defer`
gate + types `infer_ident` in value position as a **dedicated `Ty::BuiltinFn { params, ret }`** (from
`builtin_sig`) for ALL FOUR uniformly. `BuiltinFn` is distinct from `Ty::Func` so it is BOTH sendable
(`sendable_rec => true` — a plain `Func` is conservatively non-sendable) AND, unlike `Ty::Unknown`,
rejected by `expect_bool` (so `if print:` is a type error, not a VM-truthy/interp-fault divergence); it
is HOF-compatible with a matching `fn(...)` param via `compatible`. Because `BuiltinFn` carries a fixed
signature, the **value form of `print` is a fixed 1-arg call** — the variadic/`sep=`/`end=` surface
stays direct-call-only (a bound value can't reach `CallPrintSep`). A **user binding shadows** these
names in value position: `is_reserved_name` bans only `fn`/type/import-alias decls (NOT `ord := 5`,
`fn f(ord: int)`, `for chr in xs`), so both runtimes match the checker by resolving
locals/captures/globals BEFORE the first-class arm (compiler `compile_ident` guards `LoadBuiltin` on
`resolve_local`/`captures`/`globals` misses; interp `eval` Ident tries `env.get` first); a same-named
**module global read before its definition line** is a use-before-def error (checker suppresses the
first-class arm when the name is in `module_global_lets`), matching a non-builtin `x := y` before `y`
— this closes a VM(`nil` slot)/interp(`Value::Builtin`) divergence. Compiler emits `Op::LoadBuiltin`
**only** for unbound value-position uses — DIRECT calls (`print(x)`, `ord(c)`) are intercepted before
the value fallthrough and keep their specialized `CallPrint`/`CallPrintSep`/`CallBuiltin` opcodes, so
the hot path + benches are untouched (no bench run needed). VM/interp `invoke_value`/`call_value` route
the value by name into the SAME logic direct calls use: `print` → space-join + trailing `\n` (arg kept
**GC-rooted on the operand stack** while stringifying, mirroring `do_print`); `ord`/`chr` →
`builtin_ord`/`builtin_chr`; `panic` → the recoverable `RuntimeError` (`Err`, never `Ok`) so defers
still unwind through a `panic()` value. Builtin-value **equality compares by name** on both engines (VM
`values_equal_guarded` gained an `(Obj::Builtin, Obj::Builtin)` arm — each `LoadBuiltin` allocs a fresh
handle, so identity was wrong; interp already name-compares via derived `PartialEq`). Two-engine
(three-engine incl. M:N) parity is byte-identical. Golden `examples/defer_builtin_value.chz` (+
`.expected`) exercises the behaviors on VM == `.expected` == interp == M:N; unit tests: rewrote
`defer_builtin_rejected` → `defer_builtin_accepted`, kept `defer_constructor_rejected`, added
`defer_type_rejected` / `type_name_not_firstclass_value` / `firstclass_builtin_fn_value_position` /
`panic_as_value_uncaught_raises_both_engines` / `ord_chr_as_value_both_engines` + regression guards
`print_value_not_usable_as_bool_condition` / `print_value_form_is_fixed_arity` /
`use_before_def_global_shadowing_builtin_rejected` / `user_binding_shadows_firstclass_builtin_typechecks`
(checker), `builtin_value_equality_both_engines` / `builtin_value_sendable_across_airlock_both_engines` /
`user_binding_shadows_firstclass_builtin_both_engines` / `print_as_value_arg_rooted_under_gc_stress`
(VM==interp==M:N). Docs: `docs/syntax.md` §`defer` (first-class list + value-form 1-arg limit +
sendable + shadowing + use-before-def). **Post-review parity fixes (2026-07-01):** (1) a first-class
builtin spawned as a **call callee** (`f := ord; spawn f("a")`, and bare `spawn print("hi")`) faulted
`spawn: 'function' is not an isolable task` on the **M:N engine only** — `prepare_worker`'s
`PendingCall::Call` arm handled only `Closure`/`Func`; added a `Lowered::Builtin` arm (crosses by name,
worker re-allocs `Obj::Builtin`), restoring three-engine parity. (2) The `spawn` gate now **accepts**
first-class builtins (symmetric with `defer`), and (3) `sep=`/`end=` on a deferred/spawned `print` are
a **type error** (the value form can't carry them) instead of being silently dropped. Tests:
`spawn_builtin_fn_value_as_call_callee_both_engines` / `spawn_bare_builtin_print_both_engines`
(VM==interp==M:N), `spawn_firstclass_builtin_accepted_like_defer` / `defer_spawn_builtin_named_args_rejected`
(checker). **What's next:** unchanged — M19 perf Tier-1 (method-call IC,
`run_until` trim, `Op::Call` specialization).

**✅ Front-end — deep iterative-chain host-crash backstop (pre-JIT audit, 2026-07-12, `fix/frontend-deep-stack`).**
A *valid, well-typed* program with a long **left-associative binary chain** (`1+1+…`) or **postfix chain**
(`x.f.f…`, `a[0][0]…`, `f().g()…`) — ~4000+ folds — aborted the host (**SIGABRT, exit 134**) at compile
time on both `check` and `run`, both engines, uncatchable by `recover:`. Root: these forms parse in the
**iterative** `while`/`loop` bodies of `parse_bp` / `parse_postfix`, which never bump `self.depth`, so the
`MAX_DEPTH` guard never fires — yet each fold adds one level to a left-leaning AST that the **post-parse
recursive walkers** (`desugar::walk_expr`, checker inference, compiler lowering) then overflow on. This is
the sibling of the deep-*pattern* fix below, but on the *walker* axis (parse succeeded; the `ast` dump
does not crash). Two-part fix: (1) a `MAX_CHAIN_DEPTH` (500) **per-loop** counter in both iterative loops
caps a single chain, so the accepted AST depth is bounded (`MAX_DEPTH` × `MAX_CHAIN_DEPTH`) — sibling
BREADTH (a 5000-element list, a wide arg list) is *not* conflated with chain depth; (2) the whole
front-end now runs on a dedicated large stack — `chezzi::on_frontend_stack` / `FRONTEND_STACK_BYTES`
(1 GiB), plus `run`'s build+compile already on the VM's 384 MiB `VM_STACK_BYTES` — so the bounded depth is
walked with headroom regardless of the caller's stack. A parser cap **alone** cannot fix it: any cap high
enough for real code still overflows the ~2 MiB LSP tokio-worker walk (the `recursion-guard-smallest-stack`
rule), so the large stack is load-bearing. `MAX_DEPTH` unchanged. Coverage: parser
`deep_iterative_chains_error_not_crash` (binary + postfix over-cap reject, under-cap + wide-breadth accept),
`vm::deep_accepted_chains_run_without_stack_overflow` (a ~12k-deep accepted AST runs on the 384 MiB VM
debug stack), CLI `deep_chains_never_signal_crash_the_host` (the 6000-fold repro exits with a code, never a
signal). Docs: `docs/bug-discovery.md` deep-nesting section.

**✅ Parser — deep-nested-pattern host-crash backstop (pre-JIT audit, 2026-07-12).** A `match` arm
with a deeply-nested pattern (`Some(Some(… ))` variant payloads, or `((( … )))` tuple elements)
recursed `parse_pattern_impl` (`src/parser/mod.rs`) with **no depth guard** → host **stack overflow /
SIGABRT** (`check`/`ast` exited 134, before either engine — so parity is moot). The pattern parser was
the un-guarded **fifth** recursive entry point: the other four (`parse_stmt`, `parse_type`, `parse_bp`,
`parse_unary`) already cap at `MAX_DEPTH` (64). Fix wraps `parse_pattern_impl` — the per-level
chokepoint both `parse_pattern` and `parse_subpattern` funnel through, so one guard caps every nesting
axis (variant + tuple + or-alt) — in the same `self.depth += 1; if self.depth > MAX_DEPTH { return
Err(self.err("pattern nested too deeply")) } … self.depth -= 1` idiom, mirroring `parse_unary` exactly
(decrement on the success paths, deliberate leak on the early `?`/guard-Err path — the callers never
backtrack). `MAX_DEPTH` unchanged. Now a pathological pattern prints a clean `pattern nested too deeply`
parse error and exits 1 (no 134). Regression coverage extended `deep_nesting_errors_not_crash` (now all
**five** entry points: deep-variant + deep-tuple pattern cases) + a legal depth-30 nested-`Option`
match VERIFY test (checker exhaustiveness/type-walk + VM matcher stay safe, no over-rejection). No doc
change — no doc claimed patterns were depth-unbounded.

**✅ Resolver — deep-import-chain host-crash backstop (pre-JIT audit, 2026-07-01).** A pathological
*acyclic* linear import chain (~8-10k modules deep) recursed the resolver's DFS `Builder::visit`
(`src/resolver/mod.rs`) with no depth limit → host **stack overflow / SIGABRT** (`check` exited 134;
`run` printed `thread 'main' has overflowed its stack / fatal runtime error`). Import *cycles* were
already caught cleanly; this closed the acyclic-but-very-deep hole. Added `const MAX_IMPORT_DEPTH =
2000` (test-overridable via a `Builder.max_depth` field) guarding `visit` **after** the cycle+visited
checks — so cycle detection and diamond dedup are unregressed, and only DEPTH (`on_stack.len()`) is
bounded, not breadth. Exceeding it returns a clean `import chain too deep (exceeds 256)` diagnostic
attributed to the offending import (same shape as the cycle/missing-module arms). The checker's module
walk (`run_graph_pass`) iterates the resolver's already-flattened `graph.modules` linearly — no
independent recursion, so the single resolver guard covers both `check` and `run` (they funnel through
`resolver::build_graph`). Verified end-to-end on the 8MB main thread: a generated 2100-deep chain now
prints the clean diagnostic and exits 1 (no 134/SIGABRT) on both paths. TDD with an injected small
limit (the test-harness worker stack is far smaller than main — a real 2000-deep test would overflow
the *test* thread, per `parser::MAX_DEPTH`). Docs: `docs/spec.md` §Imports.

**✅ Turbofish construction on the value-first concurrency boxes `Shared`/`RwShared`/`Atomic`
(checker-only, 2026-06-30).** `Shared[int](0)` / `RwShared[Map[str, int]]({…})` / `Atomic[int](0)`
now type-check; the turbofish is **optional** (value-first inference still works with no type arg) and
when present **pins the element type, checked against the value** — `Shared[str](0)` is a type error
(`Shared[str]() expected element type str, found int`), and arity > 1 (`Shared[int, str](0)`) is
rejected. Reverses the prior "left OUT — `Shared`/`RwShared`/`Atomic` reject a `[T]` type arg" stance
of the container-ctor turbofish work. Two edits in `src/checker/mod.rs`: add the three names to the
`name_is_generic` whitelist (so a turbofish call clears the `'…' takes no type arguments` gate), and
route each value-first ctor arm through a new `concurrency_turbofish_elem` helper that mirrors the
`List[T]([…])` element-check pattern. **Runtime ctor/opcode dispatch UNCHANGED** (checker-only), so
VM↔interp (+ `--parallel`) parity holds by construction: `examples/shared.chz`/`atomic.chz`/
`rwshared.chz` converted to the turbofish form with **unchanged `.expected`**, exercised on both
engines by the existing goldens; value-first runtime stays covered by `examples/parallel_shared.chz`.
Out of scope (untouched): the global `Result`/`Option` ctors `Some`/`Ok`/`Err`, and `Executor`
(non-generic, stays rejected). Docs synced: `docs/stdlib.md`, `docs/syntax.md`, `docs/concurrency.md`.

**✅ Capture-sendability gate + permissive `Func` — B3.3 checker half (Task 2a, 2026-07-10, `b33-checker-gate`).**
Makes the checker consistent with the by-value airlock runtime (B3.3e, below). Two changes, checker-only
(+ one `pub(crate)` in `src/compiler/mod.rs`):
- **`sendable(Ty::Func)` flipped `false → true`** (`src/checker/proto.rs`) — a closure crosses by value,
  so the bare `fn` type is sendable. Effect: `Channel[fn(int)->int]` type-checks; a closure stored in a
  channel / returned from a factory / sent across a task now checks AND runs (verified 42 / 105 on both
  engines). `Ty::Module`/`Ty::Protocol` stay non-sendable; `Ref` (std.ref box) stays non-sendable.
- **Capture-sendability gate at spawn CALLEE + ARG sites** (`src/checker/sig.rs`) — the bare `fn` type
  can't carry its captures, so the per-closure check moved to the airlock sites. A scoped side-table
  (`capture_table`, mirrors `scopes`; `Capture{name,ty,is_ref}` in `src/checker/mod.rs`) records each
  closure/nested-fn's non-sendable **local** captures at its decl site (a `let name := fn…` RHS or a
  nested `fn name…` body), keyed by binding, using the SAME `free_names_*` over-approximation the runtime
  uses to build captures (now `pub(crate)`). At a `spawn <name>()` callee or `spawn f(<name>)` arg (Ident
  → side-table, inline Closure → analyzed on the spot) it emits the verbatim block-form error per captured
  non-sendable local. So a captured `ref` at a `spawn f()` callee/arg is now a **compile error** (was:
  check-OK + silent stale-value bug), matching the `spawn:` block form.
- **PITFALL guarded:** a **module-global** `ref` is a read-only global (scope-0 exclusion in
  `local_captures_of`), NOT a per-task capture — never gated (`module_global_ref_spawn_callee_ok` +
  runtime 42 both engines locks it; `all_shipped_examples_typecheck` incl. `examples/ref_binding.chz`
  stays green).
- **Task 2b — indirect ref-capture runtime backstop (✅ 2026-07-11, same branch):** the check-site gate
  can't see a ref-capturing closure that reaches the airlock INDIRECTLY (inside a struct field / a
  `Channel[fn]` value) — it type-checks, and used to **silently deep-copy** the ref (the write vanished).
  A VM-only backstop closes it: both closure-serialization arms (`to_wire_depth` for
  `Channel.send`/spawn args, `to_snap_depth` for the M:N module snapshot) now scan a crossing closure's
  ENTIRE capture graph via `captured_graph_embeds_ref` (`src/vm/sched.rs`) — top-level OR nested inside a
  captured `List`/`Tuple`/`Map`/`Set`/struct/enum/newtype/`Cell`/nested closure/`Iter`, `MAX_STRUCTURAL_DEPTH`-bounded
  — and a `Ref` (`Obj::Struct{name:"Ref"}`, a reserved name) anywhere in it raises the **recoverable**
  `cannot send a non-sendable ref/Ref captured by a closure across tasks — use Shared/Atomic/Channel`,
  BYTE-IDENTICAL on both engines. Scoped to the closure arms ONLY (HARD non-regression): a **module-global**
  `ref` crosses via the module-globals snapshot, not a closure capture, so it is never scanned and keeps
  deep-copying (never faults). No checker change. Together with Task 2a, **no silent `ref` path remains**.
  Tests: 3 new both-engine fault parity tests (`ref_capturing_closure_through_channel_faults`,
  `..._in_struct_field_spawn_arg_faults`, `ref_nested_in_captured_list_faults`) + 2 regression pins
  (`module_global_ref_read_in_task_still_ok`, `sendable_capturing_closure_through_channel_still_runs`).
  Full `cargo test` green, clippy clean. Docs: `docs/concurrency.md §7`, `docs/gaps.md` #1/#2, `gaps.md`.
- **Tests:** 7 new checker tests + 3 new both-engine parity run tests. Updated 9 existing checker tests
  that encoded the old "Func non-sendable" rule: `channel_non_sendable_element/struct_field_rejected` +
  `spawn_non_sendable_struct_field_arg_rejected` re-pointed at a still-non-sendable `Ref` field/element
  (the deep-sendability mechanism, not the now-sendable closure); the two `spawn_non_sendable_arg/keyword`
  tests re-pointed at ref-capturing closures (now caught by the capture gate); four block/submit
  capture-free-closure tests flipped to `_ok` (closures cross by value — verified run). Full `cargo test`
  green, clippy clean. Docs: `docs/concurrency.md §7` + A3a row, `docs/syntax.md`, `docs/gaps.md` #1/#2,
  `gaps.md`.

**✅ Executor.submit coop==M:N by value — serial-vs-M:N parity divergence FIXED (2026-07-11, `executor-submit-parity`).**
The cooperative `Executor.submit` now crosses its submitted closure **by value**, exactly like `--parallel`
and plain `spawn`. Root cause: `src/vm/netio.rs` had a coop special-case that queued the closure's own heap
`Handle` (captures shared by reference, **bypassing `to_wire`** and thus the whole airlock), while M:N used
`wire_callable`. That broke serial==M:N three ways — a submitted closure capturing a non-sendable `ref`/`Ref`
(directly or via a nested closure) or a live generator ran silently on serial but faulted on M:N, and one
mutating a captured collection observed the mutation on serial but was isolated on M:N (silent value
divergence). Fix: collapse the three-way engine-gated branch to an unconditional
`let w = self.wire_callable(args[0], span)?;` so **both** engines wire the closure by value — captures
deep-copied + isolated at submit, and the ref/generator airlock enforcement runs on the cooperative engine
too. The by-handle branch had been kept to mirror the tree-walk `interp` oracle; that oracle was removed, so
it was pure divergence. Submit-time generator reach-gate + drain-time re-gate (`gate_executor_queue`)
unchanged — reachability is proto-based over the shared `Arc<Program>`, so the queued-kind switch
`Handle`→`Closure` leaves verdicts identical; the coop inline drain already `from_wire`s each queued job, so
per-job isolation falls out. Tests (all serial==M:N): `executor_submit_ref_capturing_closure_faults_both_engines`,
`executor_submit_generator_capturing_closure_faults_both_engines`,
`executor_submit_mutating_closure_isolated_parity`, `executor_submit_sendable_closure_runs_parity` (control,
`Channel` still shared by Arc → `7`), + the rewritten `executor_cooperative_submit_isolates_captures_by_value`.
Docs: `docs/concurrency.md`, `docs/concurrency-b3.md` (C-01 superseded), `docs/gaps.md` #3 (RESOLVED),
`src/vm/netio.rs` submit comment. VM-only; checker untouched.

**✅ Closures / bare `fn`s cross the airlock BY VALUE — B3.3e runtime (2026-07-10, `b33-closures-by-value`).**
The GENERIC airlock lowering (`to_wire`/`to_snap`, not just `Executor.submit`) now crosses a closure or
bare `fn` **by value** on BOTH engines identically: `WireValue::Closure { proto, captured, home }`
(captures wired recursively in slot order, home as a `module_objs` index) and a NEW distinct
`WireValue::Func { proto, home }`. Kept separate on purpose — a bare fn renders `<fn NAME>`, a closure
`<closure>`, so collapsing Func into an empty-capture Closure would diverge the M:N snapshot-rebuild
render from the serial engine's live `Obj::Func`. Only a `Module` still crosses as `WireValue::Handle`
(its mutable globals can't cross an OS-thread heap boundary). (**Update 2026-07-23:** native `Obj::Native`
+ FFI `Obj::Cffi` fn VALUES also cross by value now — new `WireValue::Native`/`Cffi` arms — so they are
no longer `Handle`; see the native/FFI-airlock entry below.)
- **Effect:** a `spawn f()` callee whose captured environment contains a NESTED closure/`fn` (or is
  itself a bare fn) now RUNS instead of faulting at the airlock ("can't cross a worker boundary"). The
  captured plain data is deep-copied/isolated per task, matching every other sendable.
- **Touch points (VM-only):** `src/vm/wire.rs` (new `Func` variant + `has_handle`/doc), `src/vm/sched.rs`
  (`to_wire_depth` split Closure/Func/Handle arms, `from_wire` Func arm, `wire_callable` collapsed to a
  thin delegate over the generic path, `ensure_crossable` message), `src/vm/stmt.rs` (`display_wire` Func
  arm → `<fn NAME>`), `src/vm/core.rs` (`collect_core_gcrefs` Func no-op). `to_snap`/`from_snap` already
  had Func/Closure-by-value arms (M:N module snapshot) — now reached via the shared fast path too.
- **~~Preserved: cooperative `Executor.submit` by handle~~ → RESOLVED (serial==M:N by value):** the
  cooperative `Executor.submit` now crosses its closure **by value** like `--parallel` and plain `spawn`
  (`src/vm/netio.rs` routes BOTH engines through `wire_callable`; the old by-handle branch mirrored the
  now-removed `interp` oracle and was pure serial-vs-M:N divergence). Captures isolate at submit + the
  ref/generator airlock enforcement runs on both engines. See "Executor.submit coop==M:N by value" below.
- **Checker gate NOT touched (follow-up):** a function type is still non-sendable as a `Channel`/`Shared`
  ELEMENT type (`Channel[fn(int)->int]` rejected at check), and `ref`/`Ref[T]` runtime handling is
  untouched. Runtime half only. Tests: 5 new both-engine parity (`closure_as_data_into_spawn_callee_parity`,
  `nestedfn_...`, `generator_captured_...still_faults` regression pin, `bare_func_crosses_...renders_fn_name`,
  + `d_used_sibling_closure_crosses_by_value_parity` replacing the now-obsolete `d_used_non_sendable_still_faults`).
  Full `cargo test` green (3264 lib + integration + conformance), clippy clean. Docs: `docs/concurrency.md §7`,
  `docs/concurrency-b3.md` (B3.3e row), `docs/gaps.md` #1/#2.

**✅ Nested `fn` decls are first-class local functions (2026-07-10, `auto-task/nested-fn-firstclass`).**
Closed a checker/compiler soundness divergence: a `fn` declared inside a body was compiled to a
NON-capturing local (`MakeFunc`, couldn't recurse or capture) while the checker only ran its body when a
GLOBAL namesake existed (else: unchecked body, false `unknown name`, or a wrong-arity global validation
that check-passed then run-faulted). Now a nested `fn` is a **closure-with-a-name**, reusing the existing
`Obj::Cell` / `MakeClosure` machinery (no new ops):
- **Compiler (`src/compiler/mod.rs`).** The nested `StmtKind::Fn` arm routes through `MakeClosure` (was
  `MakeFunc`): a **letrec cell** for the name (`Nil; NewCell; SetLocal` → snapshot → `MakeClosure` →
  `CellStore` the finished closure) when the name is boxed (recursive / captured by a deeper sibling),
  else a plain post-snapshot `emit_decl_named`. `compile_fn` grew a `compile_fn_captured(decl,
  captured_names)` seam (top-level/method callers pass `Vec::new()` → byte-identical `MakeFunc` path).
  `find_boundary_free_block` gained a `StmtKind::Fn` arm collecting the body's free names relative to
  PARAMS-ONLY, so BOTH captured outer locals AND the self-name box in the enclosing frame (the boxing-
  completeness fix that keeps `GetCaptured`+`CellLoad` from hitting a raw non-cell).
- **Checker (`src/checker/sig.rs`).** The `StmtKind::Fn` arm now branches on `scopes.len() > 1` (nested;
  module top level is exactly one scope): build the sig via `fn_sig`, infer an omitted `-> T` via
  `infer_fn_ret` (provisional `Ty::Func` declared first so a recursive call resolves as an arity-checked
  value-call), `declare` the name into the current scope BEFORE `check_fn_body` (nearest-scope resolution
  + recursion type-check). Top-level path unchanged.
- **Capture semantics inherited unchanged** from the uniform by-reference model: reads see later writes,
  a statement body can reassign a captured local (visible in the defining scope), a captured loop var
  gets a fresh cell per iteration, and across the `spawn`/`parallel:` airlock a capture-bearing nested
  fn (an `Obj::Closure`) is deep-copied/isolated — identical to closures. **v1 limits:** no nested
  generics + no sibling mutual recursion + a nested fn may not shadow a builtin/ctor name (all clean
  compile-time rejects, never check-OK/run-fault).
- **Adversarial-review fixes (2026-07-10, same branch).** Two confirmed check-OK/run-divergent holes
  the first cut introduced: (1) the synthetic `<toplevel>` proto never computed `boxed_names`, so a
  nested fn/closure capturing a **module-top-level** `for`-loop variable snapshotted a raw int and hit
  `unreachable!("CellLoad on a non-handle value")` — panicking BOTH engines (`src/compiler/mod.rs`, the
  toplevel `fc.boxed_names = captured_names_of_body(&module.stmts, &[])`; also fixes the identical
  pre-existing top-level-closure panic). (2) the nested-fn checker branch had no reserved-name guard, so
  a nested fn named after a builtin/ctor (`print`/`range`/a struct/newtype ctor/`Ok`/`Err`/`Some`/`None`)
  was declared as a shadowing local while the backend resolved the builtin — reintroducing the exact
  divergence (`src/checker/sig.rs`, guard on `is_reserved_name`/`struct_names`/`newtype_names`/builtin
  variants; user-enum variants excluded — not bare-callable, no divergence).
- Tests: 11 checker (`nested_fn_*`/`nested_generic_fn_clean_reject` + 4 `nested_fn_shadows_*_rejected`,
  entry/graph path) + 7 runtime parity (`nested_fn_recursion_parity`, capture read/write, loopvar-fresh
  in-fn + top-level ×2, spawn-airlock-isolated). Full `cargo test` (3247 lib + integration + conformance)
  green, clippy clean, serial==M:N. Docs: `docs/syntax.md` new "Nested function declarations" section.

**✅ Finding D — free-variable capture (over-capture soundness fix, 2026-07-10).** Every `MakeClosure`
site captured via `fc.snapshot_entries()` = ALL locals visible in the enclosing frame, so an unused
non-sendable sibling (a closure value / live generator the task never touches) was dragged across the
`spawn` airlock (`prepare_worker`→`ensure_crossable`) and faulted with "spawn: this task value can't
cross a worker boundary yet" — even though the body never referenced it (`chezzi check` OK, `chezzi run`
+ `--serial` fault). Fix: each `MakeClosure` site now captures **only the body's free-variable set**,
computed by the existing trusted over-approximation (`free_names_expr` for closure-expr bodies,
`free_names_block` for statement bodies — the same analysis that drives cell-boxing), via a new
`filter_entries_free_block` helper. All five sites converted (compile_closure, both nested-fn arms,
`spawn:` block, `defer:` block); `entries` and `captured_names` derive from the SAME filtered vec so
positional `GetCaptured` slots stay aligned. Behavior-identical + strictly smaller capture: a global
free name is still read live via its global slot (never in `snapshot_entries`); the LETREC recursive
self-name is free in a recursive body → stays captured → self-call resolves. User-visible semantics
UNCHANGED (still uniform by-reference) — it just no longer drags unused siblings. 13 new both-engine
parity tests (`vm::parity_tests::d_*`): #D repro (unused sibling closure-value + generator variants,
nested-fn + closure-value spawn), used-sibling-still-faults (no over-narrow), spawn:/defer: block, and
a no-under-capture battery (method call on captured receiver, interpolation-only ref, grandparent
through two frames, recursion self-ref, ref-capture mutate, match+comprehension, non-recursive nested
fn). Full `cargo test` green, clippy clean, serial==M:N. Bench-neutral (benches don't stress capture).
Docs: `docs/syntax.md` capture section, `src/vm/op.rs` CapSrc comment.

**✅ Uniform by-reference capture — Task B WIRED (semantics + airlock, 2026-07-09).** Closure/`defer:`/
`spawn:` capture is now **uniformly by reference**: a capturing frame shares the closest binding of a
captured name and sees/makes writes to it (was: plain local snapshotted by value). Builds on Task A's
`Obj::Cell` primitive + capture pre-pass. Full `--lib` 3215 green, both-engine parity clean, clippy clean.
- **Boxing + routing (`src/compiler/mod.rs`).** A local/param captured by a nested closure/`defer:`/
  `spawn:` is a BOXED slot: declared with `NewCell`, read via `CellLoad`, written via `CellStore`. B1
  safety spine: **exactly four emit fns touch a slot** — `emit_hidden_get`/`emit_hidden_set` (raw +
  debug-assert the slot is unnamed) and `emit_get_named`/`emit_set_named` (cell-aware); ALL named binding
  sites (`:=`, tuple/field destructure, `match`/`if let` bind, `wait`-assign, loop var, comprehension
  acc) route through them, so a bare `GetLocal` on a boxed slot (which the peephole fuser would read as
  an int → crash) is a compile-time impossibility. `emit_store` gained the captured-write branch
  (`GetCaptured; CellStore`), fixing the old phantom-global write bug (A2). Captured **loop vars** get a
  FRESH cell per iteration (C1, Go ≥1.22) via a hidden mechanism slot + `emit_loopvar_refresh`. Pre-pass
  fix: `find_boundary_free`/`collect_frame_binds` now descend into `Str` interpolation exprs AND
  expression-position `match`/`if`/comprehension bindings (a capture through either was previously
  unboxed → `CellLoad`-on-raw crash).
- **Airlock (`src/vm/{wire,sched,core,mod,stmt}.rs`).** `Obj::Cell` crosses the airlock by DEEP COPY to a
  fresh independent cell on BOTH engines (`WireValue::Cell`/`SnapValue::Cell` + `has_handle`/
  `collect_core_gcrefs`/`value_embeds_generator` arms), so a plain captured local sent into a `spawn` is
  an isolated per-task copy — **F1, the one deliberate divergence from Go** (`spawn: x=x+1; print(x)` →
  `0`, not `1`; the memory-safety line). Cross-task shared mutation still requires `Shared[T]` (F2/F4).
- **Checker (`src/checker/sig.rs`, minimal).** Lifted the obsolete by-value gates: a `defer:` block may
  now reassign a captured local (same task, shared cell); a `spawn:` task may reassign a captured LOCAL
  (isolated per-task copy — F1) but a captured MODULE GLOBAL stays rejected (frozen under `--parallel`,
  would diverge serial/M:N). No type-system change (cells are type-invisible; a boxed `x:int` still types
  as `int`).
- **Cell-bearing closure crosses `spawn` by DEEP value (B2/F3, `src/vm/sched.rs`).** A `spawn f()`
  callee that captures locals (uniformly cells) is deep-copied over the task boundary
  (`do_spawn` → `cross_spawn_callee` → the existing `wire_callable`/`from_wire` round-trip), snapshotting
  its cells at spawn time on BOTH engines — matching the M:N `prepare_worker`/`to_snap` deep-copy. A
  capture-free callable keeps the cheap shared handle. This closes a real serial-vs-M:N divergence the
  first cut missed: a closure that MUTATES a captured cell's inner heap value via a method call
  (`f := fn(): xs.push(2)`) or READS a cell the owner writes after `spawn` printed `[1, 2]`/`5` on serial
  (shared handle) vs `[1]`/`0` on M:N — now both engines isolate. Guards: `capture_spawn_closure_mutates_isolated`,
  `capture_spawn_closure_owner_write_isolated` (the latter nested to force the M:N eager path). (The
  cooperative `Executor.submit` was later brought to the same by-value semantics — see "Executor.submit
  coop==M:N by value" below — so submit now isolates captures at submit time on both engines too.)
- **Acceptance matrix** `examples/capture_*.chz` (A1/A2/A3/B1/C1/C2/D1/D2/E1/F1/F3/G1/G2/F2/F4/B6) +
  golden + serial==M:N parity twins. D1 (`capture_escape_reader` → `42\n7`, escaping capture outlives
  the frame) and D2 (`capture_recursion_percall` → `0\n1\n2\n3`, per-call fresh cell) close the last two
  §4 rows. NOTE: the design's owner-write A2/A3 rows are also re-expressed via `defer:` blocks / reads,
  because **Chezzi closures are expression-only** — a closure VALUE cannot *reassign* a captured scalar
  cell (only mutate its inner heap value via a method call, or read it); D1's Go increment-and-return
  counter is likewise inexpressible (only the escape property ships). **Docs migration (Task C) landed**
  (`docs/syntax.md` capture section rewritten, `docs/spec.md` migration note, `future.md`/`bug-discovery.md`/
  `benchmarks.md` + example comments; stale-claim grep clean).

**✅ Closure-capture model documented + golden-locked, pre-JIT (docs + golden only, 2026-06-30).**
No engine/checker change — the engines already implement the rule (`src/compiler/mod.rs:1604-1620`
`emit_load`: a local → `GetCaptured` snapshot, a global → `GetGlobalSlot` live read). Pinned the
**capture-by-binding-kind** rule before the JIT can freeze it: a **plain local** is captured **by value**
(snapshot at closure creation → `10`), a **global** is **not captured** but **referenced live** (current
value each call → `20`), and a **`ref` local** is captured **by reference** (shared box → `20`). New
three-engine golden `examples/closure_capture_scopes.chz` (+ `.expected` `10/20/20`) and its twin
`#[test] golden_closure_capture_scopes_chz_matches_expected_and_interp` in `src/vm/mod.rs` (VM ==
`.expected` == `interp::run_file` == `run_file_parallel`; runs via `run_file` through the real module
graph because the example uses `import std.ref` for the `ref int` annotation, which
`compile_module_standalone`/`run_capture` does not resolve). Reworded the over-claiming uniform-"snapshot"
header in `examples/closure_capture.chz` (and narrowed the `examples/edge_cases.chz` capture comment) to
the precise local/global/`ref` rule. Docs: a **capture subsection** (3-row table + example pointer) in
`docs/syntax.md` next to `ref T`. **Plus two doc-only clarifications:** (a) **float formatting never uses
scientific notation** — a plain `print`/`str`/`{x}` always renders the full decimal expansion
(`1.0e20` → `100000000000000000000.0`, `1.5e-9` → `0.0000000015`), shortest-round-trip-correct but verbose;
an intended Python-feel divergence, with `:e` available when an exponent is wanted (`docs/syntax.md`).
(b) **single project root** — `find_root` runs once on the entry and governs every import in the graph;
a nested `chezzi.toml` in a subdirectory is silently ignored (not a second root), so a root-level file
silently shadows a same-named subdir file (`docs/spec.md`). *(Correction — the nested-marker claim was
wrong: `find_root` DOES stop at the nearest `chezzi.toml`, and bare `chezzi run` re-derived a second
root from the entry file, which could silently disagree. Both fixed in the root-disagreement entry at
the top of this file.)*

**✅ Import-alias reserved-name gate + entrypoint-segment trim (diagnostic-only, 2026-07-01).** Two
disjoint checker/CLI-only fixes; no engine code, so VM↔interp parity holds by construction.
**FIX A — import-alias forms now honor the reserved-builtin-name guard** (`src/checker/mod.rs`,
`bind_import`). The guard that rejects `fn int()` / an extern named `int` as `reserved (builtin)` was
NOT applied to either import-alias form, so a reserved builtin *callable* could be silently rebound:
`import sqrt as int from std.math` was accepted, then the builtin `int()` conversion silently won and
the `as int` binding was dead (a SILENT WRONG RESULT — `print(int(9.0))` printed `9`, not `3.0`); and
`import std.math as int` was accepted, then failed with the confusing `module int is not callable`.
Both alias targets (`import M as X` and `import Y as X from M`) now run `is_reserved_name` and reject
`import alias 'X' is reserved (builtin)`. BOUNDARY held: value-level local shadowing (`range := 5`, a
fn param named `range`) goes through `declare` not `bind_import` and stays legal; the `a != member`
guard keeps a reserved member imported UN-aliased (`import Shared from std.concurrency`) / self-renamed
legal. Tests: `import_alias_to_reserved_int_from_rejected`, `import_module_as_reserved_int_rejected`,
`reserved_name_local_shadow_still_ok` (all via the `entry_*` build_graph→check_graph path).
**FIX B — entrypoint path segments are now whitespace-trimmed** (`src/main.rs` `entrypoint_file`).
Segments were trimmed only for the emptiness check but the RAW segment fed `module_file`, so
`entrypoint=" app "` slipped through to `<root>/ app .chz` → `cannot read ' app .chz'`. Each segment is
now trimmed before the path is built, so `" src . main "` resolves to `src/main.chz`; a whitespace-only
segment (`"a. .b"`) still trims to empty and is rejected. Test: `entrypoint_file_validates_dotted_path`
extended with the trim asserts.

**✅ Resolver diagnostic-quality fixes (diagnostic-only, 2026-06-30).** Two message/JSON fixes in the
module resolver error path; the accept/reject set is unchanged (resolve errors fire before any engine
runs, so two-engine parity is structurally untouched). **Bug 1 — missing-module / bare-`std` errors now
name the importing module.** A bad `import` inside a NON-entry module (e.g. `deep.chz` imported by
`main.chz`) previously printed `cannot find module 'x' (line N)` with no hint which file `line N` is in;
now it carries the same `in module 'deep':` prefix the parse/type errors use (via the existing
`prefix()` helper keyed on `on_stack.last()` = the importer). Entry-level imports stay unprefixed
(matches type-error attribution). **Bug 2 — `check --errors=json` resolve-error shape now matches
type-error JSON.** It previously emitted `{"message":"resolve error (line N, col M): ..."}` (the Display
prefix doubled into the message, redundant with the `line`/`col` fields); now the JSON `message` is the
clean body (with the Bug-1 `in module 'X':` attribution), while plain-text output keeps the
`resolve error (...)` Display prefix byte-identical. Implemented by carrying a clean `message` field on
`CheckOutcome::Fatal` alongside the rendered `text` (`src/main.rs`), JSON uses `message`, plain uses
`text`. New tests: `resolver::{missing_module_in_imported_module_names_importer,
bare_std_in_imported_module_names_importer}` + a negative entry-level guard on
`missing_module_is_clean_error`, and integration `tests/check_errors_json.rs` (CLI JSON shape +
plain-text via `env!("CARGO_BIN_EXE_chezzi")`).

**✅ Qualified type as static-method receiver + two-level-path diagnostics (additive, 2026-06-29).**
Two small ADDITIVE qualified-path improvements, break nothing.
**Part 1 — `module.Type.static_method()` now works** for cross-module struct AND enum statics
(`counter.Counter.zero()`, `col.Color.first()`), closing an arbitrary asymmetry (qualified
*construction* `module.Type(args)` already worked, but the qualified *static call* errored "module has
no member 'Type'"). Mirrors the bare `Type.static_method()` path exactly: checker adds a qualified
struct-static arm + reuses the qualified-enum-variant arm's no-variant fallthrough → `infer_static_call`
(variant-first preserved); compiler adds a Field-over-Field arm emitting the SAME `Op::CallStatic` keyed
by `type_key`; interp extracts `lookup_static_method_by_key` and adds the parity twin. Negative
`module.Type.no_such()` → "type 'Type' has no static method 'no_such'". Newtype statics stay unsupported
(struct/enum-gated); declaring one is now **rejected with a clear "not supported yet" error** at the
decl site + any `Newtype.method()` call site (was a cryptic "unknown name" — see the checker-leniency
note in Current focus).
**Part 2 — clear two-level-path diagnostics** for the natural 3+-level mistake (import paths *are*
multi-level, so users assume type refs are too). TYPE position (`x: std.concurrency.Shared[int]`): the
parser detects a third `.` after a qualified type and emits the targeted hint instead of cryptic
"expected '=', found '.'". EXPR position (`std.concurrency.Shared(0)`): a new checker `import_path_heads`
map (head segment → dotted path + bound name, populated in `bind_import`) turns the misleading
"unknown name 'std'" into the two-level hint; narrow — fires ONLY for a literal import-path head, never a
real typo. No grammar.bnf change (both surfaces stay two-level; Part 1 is an existing parse, Part 2 is
error-text only) — conformance green. Tests: `src/checker/mod.rs` graph_tests
(`qualified_type_struct_static_ok` / `_enum_static_ok` / `_unknown_rejects` + 3 KEEP-WORKING regressions +
`multilevel_expr_*` positives/negatives), parser `multilevel_type_path_two_level_hint`, three-engine
golden `examples/qualified_static/` (VM/interp/M:N byte-identical). Docs: `docs/syntax.md`.

**✅ First-class native (Rust-implemented) types — qualified / aliased module-member path (additive,
2026-06-29).** The import-gated native types/ctors — `Shared`/`RwShared`/`Atomic`/`Executor`
(std.concurrency), `Socket`/`Listener` (std.net), the FFI widths `int8`..`uint64` + `ptr` (std.ffi),
and `timer` (std.time) — are now reachable by the **two-level qualified / aliased module path**, exactly
like a `.chz` module type (`geo.Point`) or `regex.Match`: `concurrency.Shared[int]` / `concurrency.Shared(0)`,
`import std.concurrency as c` → `c.Shared(0)`, `type S = concurrency.Shared[int]`,
`newtype MyS[T] = concurrency.Shared[T]`, `net.Socket` annotation, `ffi.int32` (incl. inside an `extern`
signature), `time.timer(0)`. **ADDITIVE** — the existing bare-after-import licensing
(`imported_concurrency`/`_net`/`_ffi_types`/`_time`) is byte-unchanged, examples/grammar.bnf untouched,
and the import gate stays sound (qualified access to a non-imported module is still `unknown module`).
Implementation (all small/localized): (1) checker `resolve_type` `Type::Qualified` arm maps a
`sig.types` builtin name → its builtin `Ty` (shared helper `qualified_builtin_ty` + arity check; `timer`
in type position → "function, not a type"); (2) `resolve_qualified_ro` mirrors it for the RO export path
(exported alias/newtype bodies); (3) `resolve_ctype_d` `Type::Qualified` arm maps `ffi.int32`/`ffi.ptr` →
`CType::Int32`/`Ptr` for extern sigs; (4) `infer_call` Field-callee qualified-ctor arm delegates to
`infer_named_call` (Socket/Listener/widths/ptr → "has no constructor" reject); (5) compiler Field-callee
arm lowers `module.Ctor(args)` to the SAME opcode as the bare name, keyed on
`program.modules[tidx].native` (NewShared/NewRwShared/NewAtomic/NewExecutor/NewTimer); interp gets a
parity twin (`construct_native_ctor`). `bind_import` skips (VM + interp) untouched — a qualified ctor
lowers to an opcode, no runtime module-member lookup. **TWO-LEVEL ONLY** (parser is two-level for every
module; `std.concurrency.Shared` is out of scope). **Future:** retiring the bare-name licensing is its
own later milestone (one-way ratchet). Tests: `src/checker/tests.rs`
(`qualified_native_type_annotation_resolves` / `qualified_native_ctor_call_infers` /
`alias_and_newtype_over_qualified_builtin` / `ffi_qualified_width_in_extern_sig` + unlicensed/timer/Socket
negatives), three-engine goldens `examples/native_qualified.chz` (VM/interp/M:N) and
`examples/ffi_qualified.chz` (CLI-verified libc `abs(ffi.int32)`). Docs: `docs/syntax.md`, `docs/stdlib.md`.

**✅ Checker — Closure-parameter type inference (v1) + structural-match-over-`Unknown` soundness close
(2026-06-28).** Checker-only, three-engine-parity-safe by construction (rejected programs never run;
accepted programs byte-identical). **Supersedes** the earlier `MatchKind::Skip`/`OpenScrutinee`
exhaustiveness patch (that `OpenScrutinee` variant is **removed**). Unannotated closure params used to
infer as `Ty::Unknown` — the only place `Unknown` reached a runtime value — so call sites went
unchecked and a structural `match` over such a param **check-passed then trapped** on BOTH the VM and
`--serial` (`g := fn(x): match x: E.A: …; E.B: …` then `g(5)` → `cannot match on int`); a trailing `_`
could not rescue it (the destructure runs first). **Fix (5 phases):** (1) `infer_closure` gained an
`expected: Option<&Ty>` checking-mode — an unannotated param binds to the slot's param type — wired
through every `fn`-typed slot: call args (`check_args_range_w` → covers `Shared.update`/`RwShared`),
native list HOFs (`map`/`filter`/`fold`/`sort_by`/`sort_by_key`), and generic ctor/variant/fn/method
arg loops (`infer_generic_arg_tys` + `check_generic_arg`, re-inferring the closure against the
substituted field/param type, first-pass body errors suppressed). (2) the remaining slots —
`fn`-typed `let`/`:=`, struct `fn`-field assignment, `fn`-typed return. (3) **free**-closure inference:
a shallow body scan pins a param from (source #2) a `match` whose scrutinee is the **bare param**
(first concrete arm) or (source #3) a member access **uniquely owned by one type** — a `str`/`bytes`
method (`fn(x): x.upper()` → `x: str`) or a field/method exactly one user struct declares — not from
arithmetic/comparison/indexing or any member shared by >1 type (`x.len()` on `str`/`list`/`map`/`set`
never pins; they fit many types, so they're *checked*, never pin). (4) **§4.1
structural-over-`Unknown` reject** at `bind_subpattern` (nested tuple elements + variant/`Ok`/`Err`/
`Some`/`None` payloads, inherited by or-alts/guards) PLUS `match_kind`/`reconstruct_unknown_kind` (the
top-level residual-`Unknown` scrutinee arm) — `cannot match a <tuple|variant> pattern on a value of
un-inferable type; annotate it`; literal/range/`_`/binding sub-patterns over `Unknown` stay allowed
(value-compare/bind never traps). This **flips** the old `OpenScrutinee` accept-heterogeneous-literals
behaviour: the first literal arm now pins the scalar, so `1` + `"b"` rejects. (5) a genuinely
unresolved free closure param errors `cannot infer type of parameter 'x'; add a type annotation`.
**Soundness follow-up:** a closure passed to a **generic** slot whose type param only *it* binds
unifies that param to `fn(Unknown) -> Unknown`, so the substituted expected param type is `Unknown` —
an `Unknown` expected param is **not** a pin (it would re-open the launder-to-runtime hole:
`store(fn(a): a + 1)` for `fn store[T](x: T) -> T` check-passed then trapped on both engines). It now
falls through to the body scan / annotation rule and rejects at `check`. (Unification's first-pass
`infer_generic_arg_tys` keeps closure params `Unknown` via a `generic_arg_prepass` guard so the free
scan can't corrupt unification, e.g. `Mapped(int_iter, fn(x): x.upper())` still errors `no method
'upper'`, not an element-type mismatch.)
End-to-end verified on both engines: every reject errors at `check` (never executes — no trap); every
accept runs byte-identically (corpus: `iterable`/`iter_adapters`/`shared`/`parallel_shared`/`rwshared`/
`fn_field`). Tests in `src/checker/tests.rs` (real `build_graph`+`check_graph` CLI path) + migrated
graph_tests. Docs: `docs/syntax.md` (Closure-parameter inference + §match).

**✅ Docs + resolver polish (2026-06-28).** Two low-severity fixes: (1) `docs/syntax.md` "Generic
newtypes" `Stack[T].top` example used a Python-style **postfix** ternary
(`return None if … else Some(…)`) that does not parse in Chezzi (only the **prefix** `if c: a else: b`
conditional-expression form exists) — rewritten to `return if xs.len() == 0: None else: Some(…)`,
verified `Some(3)`/`None` on both engines (syntax.md code blocks are not conformance-executed, hence
the slip). (2) `src/resolver/mod.rs` — a bare `import std` routed through `module_file` to
`<install>/std.chz`, ignoring any project-local `std.chz` and leaking the internal install path; now
emits `'std' is a reserved namespace (import a submodule, e.g. 'std.math')` (narrow guard, submodules
like `std.math`/`std.x.y` unaffected). TDD: new `bare_std_import_is_reserved_namespace` resolver unit
test (RED → GREEN). No checker changes; parity-safe by construction.

**✅ Checker — operator overloading + protocol satisfaction on GENERIC structs/enums (2026-06-28).**
A generic type that defined an operator method (`add`/`sub`/`mul`/`div`/`mod`/`neg`/`compare`) could
**call it directly** but could NOT use the matching operator (`a + b`, `-a`, `a < b`), satisfy the
protocol (`Add`/.../`Comparable`), or flow into a protocol-bounded generic (`twice[T: Add]`) — `check`
*and* both engines rejected with `cannot apply + to Box[int] and Box[int]` / `does not satisfy Add
(method 'add' has the wrong signature)`. Non-generic types worked, and `Stringable`/`Hashable` worked
on generics (their sigs never mention the type param) — the exact asymmetry that proved it was a
generic-substitution bug, not a missing feature. **Root cause:** `satisfies_methods` (checker, shared
front-end) substituted only the protocol's own params (`pmap`) + `Self` into the comparison; the
RECEIVING type's own param→arg map (e.g. `T→int` from `Box[int]`) was never threaded, so the user's
stored method `add(self, o: Box[T]) -> Box[T]` (params kept UNsubstituted) failed
`compatible(Box[int], Box[T])`. **Fix:** build `tymap` from `ty` itself (struct via `struct_param_map`,
enum via `enum_param_map`, newtype via `newtype_type_params`) and pre-substitute it into the ACTUAL
(user) method signature before `method_matches`. Only the actual side is bound, so a genuinely wrong
sig (`add(self, o: int) -> int`) STILL fails — no laundering. The newtype operator-soundness gate
(generic newtype operators stay intentionally method-only/unreachable) is untouched (the fix lives
after that early-return). Parity-safe by construction (one shared checker; no per-engine logic). TDD:
new checker tests (generic struct/enum add/neg/compare, multi-param, wrong-sig boundary) + twin golden
`examples/generic_operator_overload.chz` (run byte-identical on VM, interp, parallel). `docs/syntax.md`
already documented this as working — the bug was the gap between spec and checker; now closed.
**Two soundness boundaries hardened in the same change** (adversarial-review findings): (1) the operator
now requires **matching type ARGS**, not just the same type name — `op_overload_result`/`ordering_allowed`
test `compatible(l, r)` (name + pairwise targs, `Unknown` still unifies) instead of `name == name`, so a
heterogeneous `Box[int] + Box[str]` / `Box[int] < Box[str]` is REJECTED (admitting it would infer result
`Box[int]` for a value built from a `Box[str]` → runtime type confusion). (2) `Comparable` is added to the
newtype operator-soundness gate: a same-newtype `<` ALWAYS auto-flows to the underlying's NATIVE ordering
(`compare_op`'s `same_newtype_keys` fast path), never a user `compare`, so a **generic newtype**'s `compare`
stays unreachable as an operator and must NOT claim `Comparable` (else check-ok / run-divergent). Both
boundaries covered by new failing-first rejection tests.

**✅ Checker — import+same-name-struct collision soundness hole closed (2026-06-28).** Checker-only,
three-engine-parity-safe by construction (rejected programs never run; accepted programs byte-identical).
The four native **struct-modeled** types (`Ref`/`std.ref`, `Match`/`std.regex`, `Response`/`std.request`,
`ProcResult`/`std.process`) slipped through the decl guard: (**NOTE (2026-07-01):** `Ref` has since been
promoted to a **reserved global** — always import-free, a user `struct Ref` is now *always* reserved; see
the top "reserved global backing the `ref` keyword" entry. The other three stay import-gated as below.) a program that BOTH imported one AND declared a
same-named `struct` passed `check` clean then **trapped at runtime on both engines** (e.g. `no field 'v' on
Ref(value=5)`) — the user layout overwrote the Builtin seed in the hoist while the runtime kept constructing/
returning the native shape. Root cause: the struct-hoist `already_defined` test (`mod.rs`) only treats a
*User*-origin prior as defined, so a name IMPORTED as a Builtin-origin layout was silently overwritten; the
enum/newtype/typealias decl paths were already closed via their `struct_names` collect-name guards. Fix
(approach (b), minimum-correct — NOT full reservation, which would break the module-owned bare-decl intent +
the origin-keyed sendability check): a new per-module `imported_builtin_types` set, populated at the two
struct-import insert sites (whole-module `import std.regex` + selective `import Match from std.regex`) keyed on
`info.origin == StructOrigin::Builtin`, consulted in the struct-hoist reserved-name gate → a same-named user
`struct` is now rejected `type 'X' is reserved (builtin)` (Socket/Shared precedent). Generalized: the gate also
closes the identical latent hole for every other import-gated std struct (Token/Parser/Heap/Deque/…). A bare
unimported `struct Ref` (no import) and a merely-similar name (`struct RefBox` with `import Ref`) both stay
legal. Tests: `import_plus_same_name_struct_decl_rejected` (all four + whole-module form),
`import_does_not_over_reject_distinct_struct_name`, `bare_struct_procresult_without_import_ok`; existing
`user_struct_response_without_import_ok` / `user_struct_match_without_import_ok` / `user_struct_named_ref_is_sendable`
/ `from_import_licenses_bare_response` stay green. Docs: `docs/syntax.md` module-owned note.

**✅ Manual feature-audit sweep — 3 correctness bugs found + fixed, playbook documented (2026-06-27).**
A structured adversarial hand-audit of the feature domains the *automated* oracles can't reach
(generics, `match`/enums, closures, protocols, namespace/import gating — `src/difftest/generate.rs`
emits none of these). Fanned out parallel agents per domain, each probing edge cases on BOTH engines +
`check`, evidence-gated. Found and fixed: **(1)** a `match` **exhaustiveness soundness hole** — guarded
arms (`A if c`) and refutable payloads (`Some(0)`, `Pair(0,y)`) wrongly closed a variant, so a
non-exhaustive `match` passed `check` then **faulted at runtime** (commits in
[`src/checker`](src/checker/mod.rs) `bind_match_arm`/`bind_subpattern`; a nested **single-variant**
payload stays irrefutable, verified); **(2)** the namespace name-leak (entry above); **(3)** polish —
NaN `-NaN` via the format-spec path, and a misleading "earlier push" diagnostic for an un-inferred
type param. All TDD, two-engine-parity-green, merged via `auto-task` → `post-merge-gate` (2821 tests).
The repeatable method — domains, per-agent protocol, bug taxonomy, and the procedure gotchas (verify the
CLI via `cargo run --bin chezzi` not a hardcoded `target/` path; an `ok()` unit test passing ≠ the CLI
is correct; adversarially verify every fix) — is now the **"Manual feature-audit playbook"** in
[`docs/bug-discovery.md`](docs/bug-discovery.md) (lever #9). Run it every pre-freeze session.

**✅ Checker — reserved-type-name `protocol` decl hole closed (2026-07-11).** Checker-only, parity-safe
by construction (rejected programs never reach the VM; accepted programs byte-identical — NO runtime/opcode
change). The SYMMETRIC mirror of the 2026-06-28 fix below: `hoist_protocol` (src/checker/proto.rs) guarded
only with `is_reserved_protocol`, never `is_reserved_type` — so `protocol List`/`protocol int`/`protocol
Result` (18 of 19 reserved TYPE names; only `Iterator` was caught incidentally, being also a reserved
protocol) type-checked clean, then a `struct`/`enum` decl of the same name was correctly rejected while the
protocol shadowed the builtin and surfaced as a self-contradictory `type int does not satisfy int` at a
generic bound (`fn g[T: int]`). Added one guard arm — `if is_reserved_type(name) { if
!self.current_module_is_stdlib { error "type 'X' is reserved (builtin)" } return; }` — ordered AFTER the
reserved-protocol early-return so `Iterator` stays a single error with protocol wording. Stdlib carve-out
reused verbatim (no native protocol is named after a reserved type). Test:
`protocol_named_reserved_type_rejected_at_decl` (19-name graph-path sweep + `Drawable`/`Eqz` non-regression
+ `Iterator` single-error). (Note: FFI `int32`/`ffi::TYPE_NAMES` deliberately NOT added to the protocol
guard — an asymmetry with the struct/enum guards, in-scope per task.)

**✅ Checker — namespace/import-gating, two more holes closed (2026-06-28).** Checker-only, parity-safe
by construction (rejected programs never reach the VM/interp; accepted programs are byte-identical — NO
runtime/opcode change). **HOLE A — protocol-name type decls:** the 15 prebuilt PROTOCOL names
(`Comparable`/`Stringable`/`Hashable`/`Error`/`Add`/`Sub`/`Mul`/`Div`/`Mod`/`Neg`/`Arithmetic`/`Iterable`/
`Index`/`IndexSet`/`Slice` — `Iterator` was already blocked incidentally via `is_reserved_type`) could be
declared as `struct`/`enum`/`newtype`/`type` alias because the five type-DECL guards consulted only
`is_reserved_type`/`ffi::TYPE_NAMES`, never `is_reserved_protocol`. A `struct Comparable` silently shadowed
the protocol and produced a self-contradictory diagnostic (*type Comparable does not satisfy Comparable
(missing method 'compare')*). Added `|| is_reserved_protocol(name)` to all five decl guards (NewType,
TypeAlias, Struct, Enum, NewType-with-methods) → now reject `type 'X' is reserved (builtin)`, uniform with
every other reserved type. The protocol BOUND (`[T: Comparable]`) and a type-PARAM named like a protocol
stay legal (only the standalone TYPE decl is reserved). **HOLE B — bare `owned_str`:** `resolve_type` mapped
`owned_str => Ty::Str` UNCONDITIONALLY, so `fn f(x: owned_str) -> owned_str` checked clean and silently
collapsed to `str` with no import (its sibling `ptr` correctly errors). `owned_str` is a RETURN-ONLY extern
marshalling form, not importable — gated by CONTEXT (not import): an `in_extern_sig` flag set around the
extern fn signature loop licenses the arm there; a bare non-extern use now errors *'owned_str' is a
return-only extern marshalling type and cannot be used as a general type annotation*. Extern returns (no
import) + the `id[owned_str]` type-param shadow + the extern-param surface guard all unchanged. Tests:
`protocol_named_types_rejected_at_decl` (15 names × 4 decl forms + the literal repro),
`protocol_bound_and_typeparam_named_protocol_still_ok`, `bare_owned_str_outside_extern_rejected`,
`extern_owned_str_return_still_ok_no_import` (graph path). Mirrors the 7241b5e/1fde673 reserved-type
precedent.

**✅ Checker — builtin-type namespace name-leak, two holes closed (2026-06-27).** Checker-only,
parity-safe by construction (mirrors the landed std.concurrency/std.time/std.ffi gates; NO runtime/opcode
change beyond two byte-identical `bind_import` skips). **HOLE A — decl-guard incomplete:** `is_reserved_type`
blocked only 8 names while `resolve_type` maps ~16 bare names to builtins, so `struct int` / `enum List` /
`struct Socket` type-checked clean then silently shadowed the builtin at the use-site. Extended
`is_reserved_type` to the full builtin scalar (`int`/`float`/`bool`/`str`/`bytes`/`bytearray`/`nil`),
container (`List`/`Set`/`Map`/`Channel`/`range`), and handle (`Socket`/`Listener`/`ptr`/`owned_str`) set,
and added the `native::ffi::TYPE_NAMES` (FFI width names like `int32`) check to the struct+enum decl guards
(mirroring NewType/TypeAlias) — all four decl forms now reject with `type 'X' is reserved (builtin)`.
**HOLE B — std.net types ungated:** `Socket`/`Listener` resolved to `Ty::Socket`/`Ty::Listener`
UNCONDITIONALLY (no `import std.net`), unlike `Executor`/`Shared`/`ptr`. Added an `imported_net` per-module
licensing set wired from the whole-module `import std.net` arm + a per-name `import Socket from std.net`
branch (rename rejected), with a `net_licensed` helper gating the `resolve_type` arm; unlicensed bare use now
errors `unknown type 'Socket' (import it from std.net: ...)`. Runtime `bind_import` skip added to BOTH vm +
interp so `from std.net import Socket` doesn't fault (the type carries no module-member value). Production
blast radius zero (all std.net examples already import it). Tests: `reserved_builtin_type_names_rejected_at_decl`,
`bare_net_type_without_import_hints_import`, `net_type_with_import_ok`,
`net_type_from_import_partial_does_not_license_other`, `net_type_rename_rejected`,
`vm::tests::net_from_import_runs_both_engines` (both engines).

**✅ Checker — reserved builtin TYPE names rejected as generic type-PARAMETER names (2026-06-30).**
Checker-only, three-engine parity by construction (rejected programs never reach codegen/runtime; no
vm/interp edits). Closed the last reserved-name discipline hole: the five decl guards applied
`is_reserved_type` only to the declared type NAME, so a type-PARAMETER named after a builtin type
(`struct Box[int]` / `[List]` / `[Result]`, `enum E[int]`, `newtype N[List]`, a method's own `[U]`,
`protocol P[int]`, the FFI width `[int32]`) type-checked clean and then shadowed kind-dependently — a
scalar param was dead/unreferenceable (the scalar wins in `resolve_type`), a container/enum-builtin
param silently SHADOWED the builtin as a real generic. This **reverses commit 9829f94** (which had
deliberately made such params shadow-and-run) to honor the one-way-ratchet rule (*a reserved builtin
type name must error `reserved (builtin)`, not silently shadow*). New `reject_reserved_type_params`
helper (predicate = `is_reserved_type` + `ffi::TYPE_NAMES`, span = the param-name token) called once
per decl at the five hoist sites (struct/enum/newtype/fn_sig/protocol — hoist-only so fired exactly
once, no double-report). Scope: reserved builtin TYPE names only — a type-param named like a prebuilt
PROTOCOL (`fn id[Comparable]`) and a protocol BOUND (`[T: Comparable]`) stay legal (unchanged,
guarded by `protocol_bound_and_typeparam_named_protocol_still_ok`). Normal `[T]` / `[K, V]` / word
params untouched. Tests: `reserved_builtin_type_names_rejected_as_type_params`,
`type_param_named_like_reserved_type_rejected` (inverted from the old not_shadowed guard),
`reserved_typeparam_fix_does_not_overreject` (boundary), `vm::parity_tests::
type_param_named_like_reserved_rejected_at_check` (inverted; full build_graph+check_graph CLI path).

**✅ Editor tooling — LSP hover for the FREE-FUNCTION decl name (2026-06-30).** Closes the lone
decl-site gap left by Tier-A point (5), which recorded a decl-name hover only for METHODS
(`record_method_decl_hover`, called from the struct/enum/newtype arms) — a FREE function name
(`fn foo(...)`) still hovered nothing, even though its params, return type, and the call site already
did. Fix is one probe-gated block in `check_fn_body` (the single funnel every free fn AND method routes
through): build `Ty::Func { params: sig.params.clone(), ret: sig.ret.clone() }` and
`hover_record_at(decl.name_span, &fty, HoverKind::Func, sig.doc.clone())` at the existing
runtime-inert `FnDecl.name_span`. For methods this is a harmless no-op — `record_method_decl_hover`
latches the receiver-stripped sig FIRST (first-hit-wins in `hover_record_at`), guarded by the unchanged
`hover_method_decl_name` test asserting `fn() -> int` with `self` stripped. No `!generic_arg_prepass`
gate is needed: `check_fn_body` never runs under the generic-arg prepass (proven green by
`hover_generic_free_fn_decl_name`, which Displays `fn(T, T) -> T` with no `?` latch). Checker/editor-only,
probe-gated → zero runtime/typecheck/codegen/VM/interp change, two-engine parity green, goldens
byte-identical, conformance unchanged (no syntax/grammar change). Tests:
`editor::tests::hover_free_fn_decl_name`, `hover_free_fn_decl_name_shows_doc`,
`hover_generic_free_fn_decl_name`, and end-to-end `lsp_smoke::hover_fn_decl_name_round_trip`.

**✅ Editor tooling — LSP hover for the ENUM-VARIANT decl name (2026-06-30).** Sibling of the free-fn
decl-hover note above: hovering a variant name at its declaration (`Val` in `enum Col:\n    Val(int)`)
showed nothing, while the USE site (`Col.Val(3)`) already hovered its ctor signature. Root cause: the
`Variant` AST node carried no `name_span` (unlike its sibling `Field`), so the token position was
unrecoverable at check time. Fix is additive + runtime-inert, mirroring `Field.name_span` /
`FnDecl.name_span`: (1) add `pub name_span: Span` to `ast::Variant` (diagnostic-only, never read by
desugar/compiler/vm/interp; derived `PartialEq` kept — identical source ⇒ identical spans); (2) the
parser captures the variant-name token span; (3) the `StmtKind::Enum` arm, under the existing
`hover_probe` guard, loops the variants and records `hover_record_at(v.name_span, &Ty::Func { params:
<resolved payload>, ret: Ty::Enum(name, targs_disp) }, HoverKind::Func, None)` — reusing the EXACT
type construction `infer_variant_call` uses at the use site, so decl-site and use-site displays agree
(`Val(int)` → `fn(int) -> Col`, generic `Full(T)` → `fn(T) -> Box[T]`, nullary `Red` → `fn() -> Col`).
No `!generic_arg_prepass` gate needed (this arm runs in `check_stmt`, not the localized inference
prepass — proven by `hover_generic_enum_variant_decl_name`). A variant has no doc field, so doc=None
(only the signature surfaces, NOT variant doc-comments). Checker/editor-only, probe-gated → zero
runtime/typecheck/codegen/VM/interp change, two-engine parity green, goldens byte-identical, conformance
unchanged (the AST-only field adds no surface syntax). Tests:
`editor::tests::hover_enum_variant_decl_name`, `hover_generic_enum_variant_decl_name`,
`hover_nullary_enum_variant_decl_name`, and end-to-end `lsp_smoke::hover_enum_variant_decl_name_round_trip`.
**Reinstall the LSP snapshot to serve it: `cargo install --path . --features lsp --bin chezzi-lsp`.**

**✅ Editor tooling — LSP hover for IMPORTED user types + GENERIC annotation heads (Tier-C follow-up) (2026-06-30).**
Closes the two "No information available" type-name hover gaps the Tier-C entry below flagged. (a) An IMPORTED
user type (`import Heap from std.collections`): its own decl docstring now crosses the module boundary — added an
editor-only `doc: Option<String>` to `StructInfo`/`EnumSigInfo`/`NewTypeSigInfo`, populated in `capture_sig` from
the defining module's `name_docs`. `bind_import`'s From-arm records the import-line token hover at the bound name
(`record_imported_type_hover`) — the type's own docstring, else a `kind (from module)` fallback (`struct (from
std.collections)`) — AND seeds `name_docs[bind]` so later bare/annotation/generic-head uses surface the same doc.
(b) A GENERIC annotation head (`xs: List[int]`, `h: Heap[int]`): the `Type::Generic` arm now carries the head-name
token span (added an equality-NEUTRAL 3rd `Span` field to AST `Type::Generic`, exactly like `Type::Named`'s span —
the hand-written `PartialEq` ignores it, no engine/codegen ever reads it) and records a probe-gated, `!generic_arg_prepass`
head hover REUSING `builtin_type_doc` for builtin heads (`List`/`Map`/…) and falling back to `name_docs` for user
heads. The existing `Type::Named` hover also gained the `name_docs` fallback, so a non-generic imported type used
as `x: Foo` surfaces its doc too. Checker + 1 AST field + parser plumbing; every doc is an `Option<String>` into
`hover_record_at` (probe-gated no-op off-probe) → ZERO runtime/typecheck/codegen/VM/interp change, two-engine parity
green, goldens byte-identical, conformance unchanged (surface syntax identical). Tests: `editor::tests::hover_generic_
annotation_head_shows_doc`, `hover_imported_type_shows_doc`, `hover_imported_generic_head_shows_doc` (+ Tier-A/B/C
regressions intact). Validated end-to-end against the worktree-built `chezzi-lsp` over JSON-RPC (the protocol the
nvim client speaks): hovering `Heap` (import line + `Heap[int]` head) and `List` in `xs: List[int]` all return the
doc. **Reinstall the LSP** (`cargo install --path . --features lsp --bin chezzi-lsp`) to pick it up.

**✅ Editor tooling — hover on the import-line token for native/reserved TYPE imports (2026-06-30).**
`import Shared from std.concurrency` (and `RwShared`/`Atomic`/`Executor`, `import Socket/Listener from
std.net`, `import ptr`/width-types from std.ffi) showed "No information available" when hovering the
imported NAME on the import line — those per-name branches license the type via the per-module sets and
short-circuit BEFORE the user-struct import arm that records a hover. New
`record_native_type_import_hover` records that token hover with the type's `builtin_type_doc` blurb (else
a `(from <module>)` fallback) and its resolved native `Ty` for display. The bare/annotation use already
worked (the `Type::Named`/`Type::Generic` hover arms read `builtin_type_doc`); this fills the import-line
gap. `import timer from std.time` is handled too — `timer` is a reserved FUNCTION (`timer(ms) ->
Channel[bool]`), so it records a function-style hover (not the type path). Imported MODULE FUNCTIONS
(`from std.rand import randint`) and VALUES already recorded an import-line hover (their signature/type;
doc only where `MODULE_FN_DOCS` covers the module — std.math/io/os today). Probe-gated/editor-only →
parity-neutral, goldens byte-identical. Tests: `editor::tests::hover_native_type_import_shows_doc`,
`hover_timer_import_shows_func_doc`. **Minor remaining gap:** a USER type-ALIAS imported by name
(`from M import Len` where `type Len = …`) records no import-line hover yet.

**✅ Editor tooling — hover Markdown escapes bare bracketed type refs (2026-06-30).** The LSP renders
the hover doc body as Markdown, so a bare type reference in a doc-comment (`Heap[T]`, `List[T]()`,
`xs[i]`) was being eaten as link syntax (`[text]` / `[text](url)` — `List[T]()` is literally an
empty-URL link) and shown as `HeapT`/`ListT`. `chezzi-lsp::escape_brackets_outside_code` now
backslash-escapes `[`/`]` that are OUTSIDE an inline code span and outside a fenced block (so
`` `List[T]` `` and fenced code stay verbatim), applied after `untag_fences` in the hover render path.
Tests: `escape_brackets_outside_code_escapes_bare_type_refs`, `..._leaves_code_spans_and_fences`;
validated end-to-end in headless nvim. **Reinstall the LSP** to pick it up. Also: `install.sh` now
installs `chezzi-lsp` (feature-gated) alongside `chezzi`.

**✅ Editor tooling — LSP hover docs for BUILTIN/STDLIB types & stdlib module fns (Tier C) (2026-06-30).**
Hovering a builtin/stdlib TYPE or ctor (`List`/`Map`/`Set`/`str`/`bytes`/`bytearray`/`Channel`/`Shared`/
`RwShared`/`Atomic`/`Executor`/`Socket`/`Listener`/`range`/`tuple`/`Result`/`Option`/`Iterator`) now shows
a concise one-line usage blurb, and — for a type with a built-in method table — an appended `methods: a, b, c`
line. The method-name lists come from authored `const *_METHODS: &[&str]` slices beside the `*_method_sig`
fns, each drift-guarded by `checker::tests::builtin_method_slices_all_resolve` (every listed name must resolve
from its `*_method_sig`, so the hover can't advertise a method that doesn't exist). New `fn builtin_type_doc(name)`
builds the blurb; it's threaded as the `doc` arg at the by-name CALL-callee hover site (covers `List[int]()` etc.)
and the bare Type-token hover site (covers `str`/`bytes`/`Executor`/bare `Shared`…), both already
`hover_probe.is_some()`-gated so the doc is built ONLY under a probe. Stdlib MODULE FUNCTIONS (`math.sqrt`…)
now hover with a doc too: authored `MODULE_FN_DOCS` slices set `FnSig.doc` (excluded from `fn_sig_eq`) inside
`native_module_sig`, surfaced unchanged via the existing `record_method_hover` — coverage is **`std.math` /
`std.io` / `std.os`** for v1 (drift-guarded by `module_fn_docs_all_resolve`); the other native modules hover
doc-less for now (follow-up). **Skipped (task-sanctioned): protocol per-method docs** — a doc-comment above a
`protocol` method sig still does NOT surface; AST `MethodSig` carries no `doc` field, so it'd need a parser +
`grammar.bnf` + conformance + new hover-site change (multi-file, out of Tier-C scope). **Known v1 gaps:**
`list.sort` and `bytes`/`bytearray.extend` are real methods handled in `infer_method_call` (not the `*_method_sig`
tables), so they're intentionally absent from the hover `methods:` lists. (The generic-annotation-head and
imported-user-type hover gaps noted here in the original entry are now CLOSED — see the follow-up entry directly
above.) Checker+editor only; every doc is an `Option<String>` passed to
`hover_record_at` (probe-gated no-op off-probe) → zero runtime/typecheck/codegen/VM/interp change, goldens
byte-identical. Tests: `editor::tests::hover_builtin_type_list_shows_methods`, `hover_builtin_type_token_str_shows_doc`,
`hover_module_fn_sqrt_shows_doc`, `hover_builtin_does_not_break_user_doc` (Tier-A fallback intact) +
`hover_struct_decl_name_shows_doc` (Tier-A regression). **Reinstall the LSP** (`cargo install --path . --features
lsp --bin chezzi-lsp`) to pick it up.

**✅ Editor tooling — LSP hover for TYPE tokens in annotations (Tier B) (2026-06-30).** Hovering a
TYPE token in an annotation now shows the RESOLVED type — `x: Id` (the `Id` → `int` if `type Id = int`),
a param type `fn f(a: int)` (the `int`), a return type `fn f() -> P` (the `P`), a struct field type
`x: int`, a `let` annotation `x: int = 5`. Almost no new code: `Type::Named { name, span }` already
carries a name-token span (its prior reader was the semantic-token overlay) and `resolve_type` already
computes the resolved `Ty` for every annotation — so the fix is a single probe-gated `hover_record_at(
*name_span, &resolved, HoverKind::Type, None)` in the `Type::Named` arm, recording at the inner
name-token span (NOT the enclosing-annotation `span` param). Gated `self.hover_probe.is_some() &&
!self.generic_arg_prepass`: the probe gate keeps off-probe checks free in this hot path (resolve_type
runs per annotation per check), the prepass gate stops the generic-arg unification prepass from
first-hit-wins latching an incomplete type. Display follows `Ty::Display`: a transparent
`type Id = int` shows `int` (consistent with the Tier-A alias-decl hover), a struct name shows the
struct, an in-scope type param shows the param. Composite inner names fall out for free — the
`Type::Generic`/`Func`/`Tuple` arms recurse into `resolve_type`, so the `int` in `List[int]` records at
its own span. New `HoverKind::Type` variant. **Known gap (partly CLOSED):** `Type::Qualified` (the `Point`
in `geo.Point`) still carries no name-token span, so it doesn't hover; the OUTER generic head (`List` in
`List[int]`) gap was CLOSED by the Tier-C follow-up (a head-name `Span` field was added to `Type::Generic`).
Inner type args hover via the recursive `resolve_type`. Checker/editor-only, zero runtime/codegen/parity impact (goldens
byte-identical). Tests: `editor::tests::hover_type_alias_transparent`, `hover_param_type_token`,
`hover_return_type_token`, `hover_field_type_token`, `hover_struct_name_type_token`,
`hover_generic_inner_type_token`, `hover_generic_fn_param_type_no_latch` (prepass-latch guard),
`hover_type_kind_is_type`. **Reinstall the LSP** (`cargo install --path . --features lsp --bin
chezzi-lsp`) — the editor binary is a snapshot.

**✅ Editor tooling — LSP hover for the five decl-site NAME tokens (Tier A) (2026-06-30).** Five
decl-site name positions that returned `None` now hover, all via the established additive-`Span`
precedent (`Field.name_span` / `Param.name_span` / `For.var_spans` / `Pattern::Ident(_, Span)`): a new
diagnostic-only span captured by the parser at the name token, then a probe-gated `hover_record_at` in
the checker — every new span is runtime-inert (never read by desugar/compiler/vm/interp), so VM↔interp
parity + all goldens stay byte-identical. (1) **type-decl name** — `struct P:` / `enum Col:` /
`newtype UserId = int` / `type Id = int` / `protocol Bar:` add `name_span: Span` to the five
`StmtKind` decl variants; the checker pass-2 arms record the decl's own `Ty` (`struct`/`enum`/`newtype`
self-ty, the aliased ty for `type`, `Ty::Protocol` for `protocol`) + the decl's doc-comment at the
name token (`HoverKind::Struct`, now PRODUCED). (2) **generic type-param decl** — `fn id[T]` /
`struct Box[T]` / method `[U]` add `name_span: Span` to `TypeParam`; the single `enter_type_params`
funnel records `Ty::Param("T")` (the bound suffix `T: Comparable` is not representable through the
`Ty`-only hover channel — bare param name only). (3) **import bound name** — `import std.math` (the
`math`), `import std.math as m` (the alias), `import sqrt from std.math` (the `sqrt`) add
`name_span`/`name_spans` to `Import::Module`/`Import::From`; `bind_import` records `Ty::Module` for the
module name and the imported fn/value type for `from`-members. `Import` gets a hand-written
equality-neutral `PartialEq` (the bound-name spans don't flip equality — `Type::Named` precedent).
From-import **type-only** members (e.g. `import Point from geo`) are not hovered (only fn/value
members) — a deliberate scope cut. (4) **assign-LHS** — `i = i + 1` records the target's type at the
simple-`Ident` lvalue span (no AST change). (5) **method decl name** — `fn dbl(self) -> int:` records
the call signature (receiver stripped for instance methods, kept for statics) at the method-name token,
matching the call-site method hover. New parser helper `parse_dotted_path_spanned` (allowlisted in
conformance — same `dottedPath` grammar). Tests: `editor::tests::hover_struct_decl_name`(`_shows_doc`),
`hover_enum_decl_name`, `hover_newtype_decl_name`, `hover_type_alias_decl_name`,
`hover_protocol_decl_name`, `hover_type_param_decl_fn`/`_struct`, `hover_assign_lhs`,
`hover_method_decl_name`, `hover_import_module`(`_alias`), `hover_from_import_name`. **Reinstall the LSP
snapshot to serve it: `cargo install --path . --features lsp --bin chezzi-lsp`.**

**✅ Editor tooling — LSP hover for the two remaining binding decl-sites (2026-06-29).** The last
two binding decl-sites that returned `None` now report their inferred type, closing out the
decl-site-hover batch (after the for-loop/param/field work): **(A) tuple-destructure** (`a, b := (1,2)`
→ hover `a` or `b` = `int`) and **(B) match-pattern binds** (`Col.Val(n)` → hover `n` = the payload
type; tuple pattern `(a, b)` → each element's type). Both follow the for-loop `var_spans` precedent
EXACTLY — purely additive, runtime-inert span metadata. (A) adds `name_spans: Vec<Span>` to
`StmtKind::Let` (parallel to `names`; `Span::default()` for synthesized/desugar lets), captured by the
parser at each binding token; `check_destructure` zips it and `hover_record_at`s each tuple-element type
(single-name let path unchanged — no regression). (B) changes `Pattern::Ident(String)` →
`Pattern::Ident(String, Span)` (the binding token's span), captured in `parse_subpattern`; the checker's
`bind_subpattern` `Pattern::Ident` arm records the hover at the binding's OWN span before `declare`. The
new `Span`s are never read by either engine (patterns route by NAME / lets lower by `names`/`value`/`ty`),
so VM↔interp parity and every golden stay byte-identical; the grammar is syntax-only (`IDENT` lists /
pattern idents) so conformance is untouched. Tests: `editor::hover_destructure_first`/`_second`,
`hover_single_let_regression` (guard), `hover_match_variant_bind`, `hover_match_tuple_bind`.

**✅ Editor tooling — LSP hover for param + struct-field DECL sites (2026-06-29).** Hovering a
parameter at its DECL site in a signature (free fn, method, OR closure) and a struct field at its DECL
site previously returned `None` (only the body USE / field-access resolved); now both report the
declared type, checker-only and probe-gated (each addition is a `hover_record_at` call — a no-op when
no probe is armed — or inside `if self.hover_probe.is_some()` → NO type-check/codegen/VM/interp change,
two-engine parity untouched). (1) **fn/method param decl** (`fn f(a: str)` → hover `a` = `str`) — one
`hover_record_at(param.name_span, …, HoverKind::Param, …)` in `check_fn_body`'s param loop, covering
free fns AND methods (both route through it); (2) **closure param decl** (`fn(a: int): …` → `a` = `int`)
— same call in `infer_closure`'s param map; (3) **struct field decl** (`struct P:\n  x: int` → `x` =
`int`) — a probe-gated loop in the `StmtKind::Struct` arm reading already-resolved field types from
`self.structs` (no re-resolve → no duplicate errors). `HoverKind::Param` is now PRODUCED (a param's
body-USE still reports `Local` — different span, first-hit-wins). The **qualified-static receiver**
(`module.Type.method()` → method sig) and **container ctors** (`List[int]()` / `List()` / `Map[K,V]()` →
display sig) were already covered (qualified-static threads through the same `infer_static_call` record;
`List[int]()` parses as a bare-`Ident` callee reaching `callee_display_ty`→`builtin_sig`) — added
regression guards. Tests: `editor::tests::hover_fn_param_decl`, `hover_method_param_decl`,
`hover_closure_param_decl`, `hover_struct_field_decl`, `hover_container_ctor_turbofish_callee`,
`hover_container_ctor_bare_callee`, `hover_map_ctor_turbofish_callee`. **Reinstall the LSP snapshot to
serve it: `cargo install --path . --features lsp --bin chezzi-lsp`.**

**✅ Editor tooling — LSP hover for value-producing call/ctor/static/receiver sites (2026-06-27).**
Hover on four call categories previously returned `None`; now they report a signature, checker-only and
probe-gated (every addition is inside `if self.hover_probe.is_some()` or routes through `hover_record_at`,
a no-op when no probe is armed → NO type-check/codegen/VM/interp change, two-engine parity untouched):
(1) **newtype constructor** (`UserId(10)` → `fn(int) -> UserId`) — a newtype branch in `callee_display_ty`
(the existing bare-Ident callee record site fires it, symmetric with the struct-ctor branch);
(2) **enum-variant constructor** (`Col.Val(3)`: variant name `Val` → `fn(int) -> Col`) — `infer_variant_call`
records the variant's ctor sig at the variant-name span (threaded a `name_span` through it + `infer_named_call`);
(3) **static method** (`Foo.default()`: `default` → `fn() -> Foo`) — `infer_static_call` records the declared
sig at the method-name span (threaded `name_span` through all four call sites);
(4) **receivers** of `Col.Val(..)` / `Foo.default()` → the enum/struct type name (`Col` / `Foo`). The
bare-builtin callee case (`print`/`range`/`chr`/…) was ALREADY covered via `callee_display_ty`→`builtin_sig`;
`len(...)` is method-only (not a free fn) so it stays an undefined-name error (out of scope). Tests:
`editor::tests::hover_newtype_ctor_callee`, `hover_enum_variant_callee`, `hover_enum_variant_receiver`,
`hover_static_method_callee`, `hover_static_method_receiver`, `hover_builtin_callee_chr`. **Reinstall the LSP
snapshot to serve it: `cargo install --path . --features lsp --bin chezzi-lsp`.**

**✅ Editor tooling — LSP hover on the for-loop binding decl-site (2026-06-27).** Hovering the loop
variable at its declaration (`for i in …` — the `i` right after `for`) now reports its inferred element
type (e.g. `int`), matching the body use-site that already worked. Root cause: `StmtKind::For` stored
`vars: Vec<String>` with no source span, so the checker had no token position to record a hover at. Fix
is purely additive metadata: a parallel `var_spans: Vec<Span>` field on `StmtKind::For` (one span per
name, mirroring `Param.name_span`/`Field.name_span`), captured by the parser via `cur_span()` before each
binding ident; the checker zips it with the declare loop and calls `hover_record_at` (a no-op unless a
probe is armed → zero overhead on normal checks). `var_spans` is never read at runtime by either engine,
so VM↔interp parity and every golden are byte-identical; comprehension-synthesized `for`s use
`Span::default()` (no decl-site hover — out of scope, intended). Tests: `editor::hover_for_binding_decl`
(decl-site) + `editor::hover_for_binding_body` (use-site regression guard).

**✅ Editor tooling — doc-comments on LSP hover (2026-06-27).** A plain `#` comment block *immediately
above* a declaration is now its DOC-COMMENT, rendered on LSP hover ABOVE the existing `chezzi` type
fence. No new marker (`#`, not `##`/`///`); multiline via stacked `#` lines (join with `\n`, one leading
`# ` stripped); **attachment rule**: the doc is the *contiguous* run of comment lines with NO blank line
between the last one and the decl — a blank line detaches earlier comments; an inline trailing comment on
the decl line is never a doc. **Lexer side-channel** (NOT new tokens): `tokenize_with_comments` captures
`(line, stripped_text)` for each comment-only line on the side, so the token stream + `chezzi tokens`
output stay byte-identical (only `resolver::parse` opts in via `parse_with_docs`; every other
`tokenize`/`parse` caller gets `doc = None`). **Coverage:** `doc: Option<String>` on `FnDecl` (covers
free fns + every method/static/associated fn since they all reuse `FnDecl`) and `StmtKind::{Let, Struct,
Enum, Protocol, NewType, TypeAlias}` (the doc is *parsed + attached* for all of these; top-level bindings
carry it and surface on hover, local bindings carry the field but are inert).
**Inert/parity:** the doc is purely informational — never read by desugar/compiler/vm/interp, so two-engine
VM==interp parity is untouched (front-end-only). **Hover wiring:** `FnSig.doc` (free fns + methods) +
`Checker.name_docs` (struct constructors + top-level bindings, simple-name keyed, entry-module-scoped like
`self.functions`) feed a 3rd element into `hover_result`/`HoverInfo.doc`; `chezzi-lsp` renders the doc as
plain markdown lines above the untagged fence. **Shadow-safe:** a `name_docs` doc surfaces only when the
hovered name actually resolves to the module top-level (scope 0) — a param/local that shadows a documented
global's name shows no doc, not the global's. **Fence-safe:** user doc text is run through `untag_fences`
before rendering, so a fenced block (```` ```lang ```` or `~~~lang`) inside a doc-comment can't reintroduce
the language-tagged fence the type fence avoids (Neovim injection crash, commit `0f36a59`). **Known v1 limit:** only `fn`/method, struct-constructor, and top-level-binding docs actually surface on
hover. The doc is parsed + attached for `enum`, `protocol`, `newtype`, and `type` aliases too, but does
NOT yet reach the popup — enum-variant constructors (`Field`-access form) and newtype constructors record
no callee hover signature (`callee_display_ty` has no enum/newtype branch), and protocol/type-alias names
have no value/expression form to hover. Separately, protocol METHOD signatures
(`MethodSig`, not `FnDecl`) get no per-method doc — only the protocol container does. Builds on the
builtin-hover plumbing below. **Reinstall the LSP snapshot to serve it: `cargo install --path . --features
lsp --bin chezzi-lsp`.**

**✅ Editor tooling — LSP hover for builtins (2026-06-27).** Hover on a builtin callee/method/stdlib-fn
previously returned `None`; it now reports a signature, via three reuse-driven cases (no flat
hand-table), checker+editor only (NO VM/interp/runtime touch → no two-engine parity risk):
(1) **builtin methods** (`str`/`list`/`map`/`set`/`Channel`/`Shared`/`RwShared`/`Atomic`/`bytes`/
`bytearray`/`Executor`/`Socket`/`Listener`) record their CALL signature off the SAME `*_method_sig`
helpers that drive inference (zero drift) via a new `record_method_hover` probe helper in each
`infer_method_call` builtin arm; (2) **stdlib-module fns** (`math.sqrt`, …) record off
`native_module_sig(module).functions` in the `Ty::Module` arm; (3) **free/ctor builtins** (`print`,
`range`, `int`/`float`/`str`, `ord`/`chr`, `panic`, `List`/`Set`/`Map`/`bytes`/`bytearray`,
`Channel`/`Shared`/`RwShared`/`Atomic`/`timer`/`Executor`) get a NEW DISPLAY-only `builtin_sig(name)`
(mirrored by hand from `docs/stdlib.md §1`; polymorphic-input slots render `?`, the concrete return is
the payload, e.g. `print`→`fn(?) -> nil`, `range`→`fn(int) -> List[int]`) consulted by
`callee_display_ty` before its `None`. **Drift guard:** `is_reserved_name` is refactored onto a
`const RESERVED_CALLABLE` slice (behavior-identical) and a test asserts every name in it has a
`builtin_sig` entry, so a future reserved builtin can't silently lose hover. `Ok`/`Err`/`Some` are NOT
reserved (user-shadowable) → still hover `None` for v1. Signature-only (no docstrings — a separate
follow-up). **Reinstall the LSP snapshot to serve it: `cargo install --path . --features lsp --bin
chezzi-lsp`.**

**✅ M22 — operator protocols (Div/Mod/Neg) + protocol embedding + `Arithmetic` (2026-06-26).** Three
new per-operator protocols wired exactly like `Add`/`Sub`/`Mul`: **`Div`** (`div(self, o: Self) ->
Self`, powers `/`), **`Mod`** (`mod`, powers `%`), **`Neg`** (`neg(self) -> Self`, powers UNARY `-`).
`int`/`float` satisfy all three intrinsically; structs/enums via the method; scalar newtypes get
`Div`/`Mod` auto-flow (Neg out of scope). Soundness: a newtype operator overload defined as a *method*
is never dispatched at runtime (the same-newtype arm always auto-flows to the underlying's native op),
so the checker does NOT satisfy `Add`/`Sub`/`Mul`/`Div`/`Mod`/`Neg` on a newtype structurally — only
via the numeric auto-flow — closing a `check`-ok / `run`-faults hole. C-style `/` truncates / `%` int-remainder, so `Div`/`Mod`
are `Self -> Self` (no float-return surprise). **Protocol embedding (super-protocols)** — a protocol
body line is now EITHER an `fn` sig OR an embed line (`Add + Sub`, order-free, interleaved); reuses
`Bound`. `ProtocolInfo`/`StmtKind::Protocol` gained `embeds: Vec<Bound>`. Satisfaction flattens
transitively (memo-free recursion, depth-cap 64) — a type satisfies P iff it satisfies every embed AND
has every OWN method; a pure bundle (embeds, no methods) short-circuits. Bound-site flattening via a
new `bound_provides` helper makes `+ - * /` legal inside an `[T: Arithmetic]` body and lets an
`Arithmetic`-bound value forward into `[U: Div]`. **Collision rules** validated declare-time (second
hoist pass, after all protocols registered so forward/cyclic refs resolve): own-fn-vs-embed = error;
same-method same-sig embed diamond dedups silently (`Arithmetic + Add` legal); differing-sig embed =
error; cyclic embed = error. Builtin **`Arithmetic`** bundle = `Add + Sub + Mul + Div`, built with the
same `embeds` field (no special-casing). `Div`/`Mod`/`Neg`/`Arithmetic` + the previously-omitted
`Error` are now reserved protocol names. Both-engine operator dispatch (vm `struct_arith` + `Op::Neg`;
interp mirror); golden `examples/arithmetic_protocol.chz` runs byte-identical on vm/interp/parallel;
grammar.bnf `protocolDecl` updated (+`tests/corpus/accept/protocol_embed.chz`, conformance green).
Surface in [`docs/syntax.md`](docs/syntax.md), [`docs/spec.md`](docs/spec.md) (M22 row).

**✅ Editor tooling — LSP server + VSCode TextMate grammar (2026-06-26).** Two highlight/diagnostic
paths, both single-sourced from the lexer so language changes flow through with no separate grammar to
maintain. (1) **`chezzi-lsp`** — a `tower-lsp` stdio language server (`src/bin/chezzi-lsp.rs`, primary
target neovim) providing **diagnostics** (lexer+parser+checker over the live buffer via
`chezzi::editor::diagnostics`, `CheckError`/`ResolveError` 1-based spans → 0-based LSP ranges, pushed on
open/change/save), **semantic tokens** (lexer `Tok` stream → base
`keyword/operator/string/number/comment/variable` legend, then an **AST-derived overlay** refines each
ident to `function` (fn-decl names + call/ctor/method callees), `type` (plain named-type references —
struct/enum/scalar names — in annotations/returns/fields/payloads/bounds; generic-constructor heads
and module-qualified names are span-less and stay `variable`, see limits), `property` (struct field
decls + field accesses), or
`parameter` (fn/closure params) — the legend is extended with those four names and a `chezzi-lsp`
`#[cfg(test)]` test asserts the server legend and `editor::SEMANTIC_TOKEN_TYPES` agree index-for-index;
a buffer that doesn't parse yields an empty overlay and degrades to lexer-only highlighting, never
erroring), and
**hover** (`K` / `textDocument/hover` → `chezzi::editor::hover`: reverses the UTF-16 cursor column to a
char column, finds the lexer token under the cursor, re-runs the SAME resolve→desugar→check pipeline as
diagnostics with a single-position checker PROBE on the entry module, and returns the inferred type of
the smallest leaf/identifier/field-name **or the signature of a call's callee** (free fn / struct ctor /
generic fn / user method — receiver stripped → `fn(int) -> int` — **and builtins**: free/ctor builtins
(`print`→`fn(?) -> nil`, `range`→`fn(int) -> List[int]`, …) + builtin-collection/concurrency methods
(`"x".upper()`→`fn() -> str`) + stdlib-module fns (`math.sqrt`→`fn(float) -> float`)) as a
```` ```chezzi <type> ```` MarkupContent — `None` only when the position has no type, lands on a bare
enum-variant callee (or `Ok`/`Err`/`Some`, which are non-reserved), or the program
doesn't check). The probe is a minimal, behavior-preserving checker
introspection (`checker::hover_type` + a `HoverKind` classification); diagnostic-only AST spans carry
the token positions — `ExprKind::Field { name_span }` plus new `FnDecl`/`Param`/struct-`Field`
`name_span`s and a `Type::Named { span }` (the latter with a hand-written equality-neutral `PartialEq`
so position never flips type equality) — all runtime-inert and parity-neutral.
**Reinstall (`cargo install --path . --features lsp --bin chezzi-lsp`) to pick up hover.** The
async deps (tower-lsp + tokio) are OPTIONAL behind a `lsp` feature with `[[bin]] required-features`, so
they never touch the default `cargo build`/`cargo test`; build on demand:
`cargo build --features lsp --bin chezzi-lsp`. (2) **VSCode TextMate grammar**
(`editors/vscode/syntaxes/chezzi.tmLanguage.json`) **generated** from the lexer's new
`KEYWORDS`/`PUNCTUATION` tables + `Token::lexeme()` — `tests/editor_tmlanguage.rs` is generator +
CI drift-guard (`UPDATE_EDITOR_ASSETS=1 cargo test --test editor_tmlanguage` regenerates; a plain run
fails if stale). Architecture: **`src/lib.rs` is the crate of record** (`pub mod editor` + the front-end module set);
both binaries are thin shims that link it — `src/main.rs` (the `chezzi` CLI) and
`src/bin/chezzi-lsp.rs` declare no front-end modules, they `use chezzi::{…}`, so the front-end
compiles **once** (its unit tests + two-engine parity + grammar `conformance` run once, in the lib
test target — no more lib+bin double-compile/double-run) — plus a
behavior-preserving `resolver::build_graph_with_entry_source` (entry from a live buffer, imports from
disk). Editor logic (`src/editor/`) is dep-free and unit-tested in the default build; the LSP server has
a `cargo test --features lsp --test lsp_smoke` JSON-RPC round-trip. Setup docs: [`editors/README.md`].
No parity risk (front-end only; never runs VM/interp). v1 limits: unsaved edits to an *imported* module
aren't reflected until saved; interpolated strings highlight as one literal (no nested `{expr}`); hover
covers the entry module only, resolves leaf idents/literals/field-names + single-name bindings +
call callees incl. **user-struct** method names, **reserved free/ctor builtins**, **builtin-collection/
concurrency methods**, and **stdlib-module fns** (generic/enum/newtype user methods,
desugared `?.`/`??`, and non-first destructuring-bind names return no type; the semantic-token overlay's
`type` role skips generic-constructor heads (`List[int]`'s `List`, `Map`/`Set`/`Box`, …) and
module-qualified type names, both span-less).

**✅ Bug-discovery lever #1 — front-end panic-fuzzer (2026-06-26).** `src/panicfuzz/` feeds
adversarial / malformed inputs to `chezzi check` (the full front-end: lexer + parser + checker) and
flags any crash. A **stable, dependency-free SUBPROCESS harness** structurally mirroring
`src/difftest/` (own `xoshiro256**` RNG copy; same reader-thread + `try_wait` + kill-on-timeout
machinery) — *not* `cargo-fuzz` (no nightly / rustup / cargo-fuzz here) and *not* in-process
`catch_unwind` (the crate is binary-only — no `[lib]` — and shelling out catches more crash classes
incl. **stack overflow**, the most likely deep-parser crash). Invariant: malformed input ⇒ a clean
diagnostic, never a Rust panic (`panicked at` on stderr) or a signal kill (exit code `None` =
SIGSEGV/SIGABRT/stack-overflow); a wall-clock timeout is **not** a finding. Three bounded (≤2 KB),
deterministic generators (`generate.rs`): random UTF-8-ish bytes; a token-alphabet sampler (Chezzi
keyword/punct/op spellings + idents/numbers/indent); raw-byte mutation of the `examples/*.chz` corpus.
A finding reports the seed + raw triggering input, reproducible via `panicfuzz --seed N` (the input is
the artifact — no shrink pass in v1). Wired as `tests/panicfuzz.rs` (classify/clean/determinism unit
guards + fuzz seeds `0..2000`) and `src/bin/panicfuzz` (`--seeds A..B`/`--seed N`/`--quiet`,
unattended). Parity is N/A (front-end crash-safety only — never runs VM/interp). `cargo test --test
panicfuzz` green (8); release sweep `0..100000` (overflow-checks OFF) and debug sweep `0..20000`
(overflow-checks ON) both **0 findings** — front-end crash-safe so far. NOTE: a *release* `chezzi` has
overflow-checks OFF so arithmetic-overflow wraps invisibly there; the debug CI gate catches overflow
panics, and a full overflow sweep needs `RUSTFLAGS="-C overflow-checks=on"`. Usage + design:
[`docs/bug-discovery.md` "Panic-fuzz harness"]. Next: Tier-1 done (#1 + #2); Tier-2/3 (proptest,
grammar-accept fuzzer, TSan/loom, coverage) remain.

**✅ Bug-discovery lever #2 — CPython differential oracle (2026-06-26).** `src/difftest/` generates
random semantically-equivalent programs over a cross-language safe subset (literals, bounded-int
arithmetic, bool/str ops, `if`/`for`/`while`, non-recursive funcs, list/map/index/len), renders each
as both Chezzi and Python from one typed IR (`ast.rs`; `emit_chezzi` + `emit_python`), runs both, and
diffs stdout (`run.rs`). The Python backend prepends a **spec shim** (`_chz_str`/`_chz_div`/`_chz_mod`)
that absorbs only the by-design surface/semantic diffs (`true`/`false`/`nil`, raw nested strings,
truncate-toward-zero `/`,`%`) — so a divergence means the impl deviated from its own contract, not a
formatting artifact. Correct-by-construction generator (`generate.rs`): well-typed, in-scope, non-zero
divisors, in-range indices, and provable i64-bound tracking so generated programs never overflow (a
Chezzi fault ⇒ real bug). Wired as `tests/difftest.rs` (P0 formatting probes + bench-pair smoke +
non-tautology guard + fixed-seed fuzz) and `src/bin/difffuzz` (unattended; `--seed N` reproduces).
3000-seed release sweep clean; manually confirmed it flags the i64-overflow class (the June-2026
`sum()` blind spot). `cargo test --test difftest` green, clippy clean. Usage + design:
[`docs/bug-discovery.md` "Differential oracle"]. Lever #1 (panic-fuzzer) now also built — see above.

**✅ DSA known-answer harness — `judge/` (2026-06-27).** A third bug-discovery oracle, complementary
to panic-fuzz (#1) and the CPython differential (#2). Where the differential generator is correct
*by construction* (safe int window, no recursion, cross-language subset) — and so is blind to exactly
those edges — this runs **hand-written competitive-programming solutions** (`judge/problems/<slug>/
solution.chz`, reading stdin) against **known-correct CSES answers**, catching *shared wrongness* both
co-developed engines agree on, with an oracle independent of both engines and of CPython. Seeded with
12 problems (11 CSES + 1 Codeforces) across distinct stress paths: `weird_algorithm` (loop/bigint
Collatz), `distinct_numbers` (Set), `missing_number` (sum bigint), `playlist` (Map sliding window),
`coin_combinations_i` (DP+mod), `counting_rooms` (grid flood-fill), `repetitions` (string iteration),
`bit_strings` (modular loop), `trailing_zeros` (true-factorial differential), `stick_lengths`
(sort+i64), `apple_division` (recursion, 2^20 calls), `cf_theatre_square` (near-i64 multiply). Cases:
committed public **samples** (`samples/*.in`/`.out`) + the
gitignored full hidden suite (`judge/data/`, the authors' IP — gated/solve-first, never committed;
drop official cases there by hand if you have them). **The harness is written in Chezzi** (`judge/run.chz`, dogfood; mirrors
`benches/run.chz`): shells out per case under `timeout`, classifies `PASS`/`WRONG`/`FAULT`/`PANIC`/
`TIME`, whitespace-normalized compare (token-sequence, CSES-accurate). Not part of `cargo test` (a
`.chz` driver). **Self-contained generated-oracle mode** (`judge/generate.py` + per-problem `gen.py` +
**independent** `reference.py` — union-find vs flood-fill, enumeration vs DP, etc. — brute force on
small inputs, fast path on large): a Chezzi-vs-Python differential needing **no download**. New
problems scaffold from their public statement via `judge/fetch_problem.py <url>` (statement + samples
+ meta; CSES/Codeforces). Generated in-domain cases run clean; negative checks confirm
`WRONG`+diff / `FAULT`+exit-code detection. **Edge-case coverage
(2026-06-27):** each problem also ships an optional `edges.py` (index protocol: no arg → count,
`argv[1]=k` → k-th input) emitting the deterministic corners random `gen.py` misses — min/max sizes,
all-equal, value extremes, exact multiples, empty/full grids (incl. `counting_rooms` 1000×1000 deep
flood-fill and `cf_theatre_square` 1e18 i64-product boundary). `generate.py` writes them as
`e{k}.in/.out` through the same oracle; 318 cases (random + edges) across 12 problems run clean. Adversarial-reviewed (4 findings
fixed: token-insensitive compare, NOEXP-as-skip, fetch stale-clear, stem-collision warn). Usage +
design: [`docs/bug-discovery.md` "DSA known-answer harness"].
Remaining: P5 (IR shrinker + corpus dump + opt-in overflow-metamorphic mode).

**✅ Oracle coverage widened (2026-06-26).** The differential oracle's IR + both emitters + generator
now cover four more construct families (granular `Features` flags `string_methods`/`slicing`/
`membership`/`tuples`, all on in `full()`): (a) the eight ASCII-identical string methods
`upper`/`lower`/`replace`/`split`/`join`/`starts_with`/`ends_with`/`contains` (`contains` renders as
Python `sub in recv`); (b) Python-style slicing `xs[a:b:c]` and negative scalar indexing on lists/
strings (both engines clamp identically — no shim); (c) `in` membership (list elem / map key /
substring); (d) tuples — literals, `.N` fields, and `a, b := t` destructuring. Only one new shim arm
(tuple stringify in `_chz_str`, kept honest by `oracle_detects_tuple_render_divergence`); every other
by-design diff is absorbed by a generator restriction, **no new allowlist entry**: `replace` `old` /
`split` `sep` forced non-empty, slice step never 0, negative index kept in `[-len,-1]`, tuple arity ≥ 2.
i64-no-overflow invariant preserved — the one new int seam (tuple-field read) inherits per-element
`tuple_bounds` and is skipped inside in-loop accumulators; method/`in`/slice results carry no int value
and `split`/slice results carry `len: None` so they're never scalar-indexed. New P0 probes + per-construct
coverage + fuzz sweeps; `./target/release/difffuzz --seeds 0..5000` clean (0 findings).

**✅ global-namespace cleanup — task 5/5 (FINAL): `list`/`map`/`set`→`List`/`Map`/`Set` HARD rename
(2026-06-25).** The three builtin container TYPE **and** constructor names are now PascalCase
`List`/`Map`/`Set` everywhere — type annotations (`List[int]`, `Map[str,int]`, `Set[int]`, nested),
turbofish, struct fields, fn params/returns, and the free-fn ctors (`List(it)`/`Set(it)`/`Set()`/
`Map(it)`). **HARD rename, no alias:** lowercase `list`/`map`/`set` as a type name now falls to the
checker's unknown-type branch (REJECTED for free — the lowercase strings simply stop matching any
`resolve_type`/`infer_named_call` arm), and as a bare name they are ordinary identifiers again.
These names were never lexer keywords nor a `Type::Named` arm — they were plain string-literal matches
in the checker (`resolve_type`/`resolve_ty_ro_d` Generic arms, `is_reserved_name`, `is_builtin_type`,
`infer_named_call` ctor arms, `newtype_aggregate_cast`), compiler/interp/vm builtin dispatch +
`is_builtin` + float-widening hints, and `json_decode` — every such literal flipped to PascalCase.
**Runtime display** flips too: `type(x)` and error text now print `List`/`Map`/`Set`, the empty-set
display is `Set()` (was `set()`), and `Ty`'s `Display`/`ref_display` render `List[…]`/`Map[…]`/`Set[…]`
(so every type-mismatch message says PascalCase) — flipped in vm + interp + checker in lockstep so
VM↔interp parity stays byte-identical. **Untouched (NOT the container type):** the `.map`/`.filter`/
`.fold` list HOF methods, the `.set` method on `Shared`/`RwShared`/`Ref`, the std.iter `map(xs, f)`
free function, `tuple` (left lowercase — possible later follow-up), internal `Ty::list/map/set`
helpers, and list/map/set **literal** syntax (`[…]`/`{…}`). TDD: `pascal_containers_resolve` +
`pascal_ctor_calls` (green) and `lowercase_containers_rejected` (lowercase now "unknown"). Migrated
~52 examples + their `.expected` goldens (empty-set `set()`→`Set()`), all `std/*.chz`, the conformance
corpus, `docs/grammar.bnf` prose, and all docs. `cargo test` (2711) + conformance + clippy clean;
three-engine parity green. **Global-namespace cleanup batch COMPLETE (5/5).**

**✅ global-namespace cleanup — `timer`→`import std.time` (2026-06-25).** The opcode-backed `timer(ms)
-> Channel[bool]` builtin is no longer global — it now requires `import std.time` (whole-module) or
`import timer from std.time` (per-name); bare use otherwise is `unknown function 'timer' (import it from
std.time: \`import std.time\`)`. Mirrors the `std.concurrency` gate but for a SINGLE opcode builtin and a
REAL native module: a NEW per-module `imported_time` set (parallel to `imported_concurrency`), populated
in `bind_import` (whole-module on the exact `[std, time]` len-2 path; per-name on the from-import,
rename-rejected), gates ONLY the `infer_named_call` `"timer"` arm via `time_licensed` (`current_module_is_stdlib`
exempts std/* — `std/cancel.chz` keeps bare use). `timer` is added to `native_module_sig("std.time")`'s
`sig.types` (NOT `func()` — opcode-backed, no runtime member) so `import timer from std.time` validates
membership. **Enforcement is checker-only** — compiler/interp/vm opcode dispatch untouched, so three-engine
parity is preserved by construction. **Two baked-in fixes:** (1) `timer` STAYS a reserved name — added to
`is_reserved_type` (`struct timer`/`enum timer` rejected) AND a NEW reserved-name guard in the `fn` hoist
(`is_reserved_name` — closes a pre-existing silent-shadow hole where `fn timer()` was dead code shadowed by
the opcode). The import gate and the reserved-name gate are SEPARATE and BOTH apply. (2) a `timer`-SPECIFIC
runtime `bind_import` SKIP on BOTH engines (vm + interp) — `module=="std.time" && member=="timer"`, NOT a
blanket std.time skip (now/monotonic/sleep_ms/format DO bind normally) — so `import timer from std.time`
(type-checks green, no runtime member) binds nothing instead of faulting `module 'std.time' has no member
'timer'`. New tests RUN both engines (not check-only): whole-module + from-import `timer(50).recv()`→`true`
byte-identical VM↔interp; plus require-import / per-name-rename-reject / still-reserved checker tests.
Examples `examples/timer.chz` + `examples/wait_select.chz` now `import std.time` (byte-identical goldens both
engines). Docs (stdlib/syntax/concurrency/CLAUDE.md) updated. `cargo test` + conformance + clippy clean.

**✅ global-namespace cleanup — task 4/5: `Shared`/`RwShared`/`Atomic`/`Executor`→`std.concurrency`
(2026-06-25).** The four runtime concurrency ctor/TYPE names are no longer global builtins — they now
require `import std.concurrency` (whole-module licenses all four) or `import Shared from std.concurrency`
(per-name); bare use otherwise is `unknown type 'Shared' (import it from std.concurrency: \`import
std.concurrency\`)`. Mirrors the FFI `ptr` machinery: a NEW per-module `imported_concurrency` set
(parallel to `imported_ffi_types`), populated in `bind_import` (whole-module on the exact `[std,
concurrency]` len-2 path; per-name on the from-import), gates the `resolve_type` arms (`Executor` +
generic `Shared`/`RwShared`/`Atomic`) and the `infer_named_call` ctor arms (`current_module_is_stdlib`
exempts std/* — `std/cancel.chz`, `std/concurrency/collection.chz` keep bare use). `std.concurrency` is
a NEW **file-less native module** (`native_name` maps len-2 `[std, concurrency]`; len-3 `import
std.concurrency.collection` still loads the file — no collision) with EMPTY callable members; its
`native_module_sig` exports ONLY the four in `sig.types`. **Enforcement is checker-only** — compiler/
interp opcode dispatch is untouched, so three-engine runtime parity is preserved by construction.
**Two baked-in fixes over the prior rejected attempt:** (1) the four STAY reserved names — `Executor`
was already in `is_reserved_type`; `Shared`/`RwShared`/`Atomic` joined it, so `struct Shared`/`struct
Executor` is now a clean at-declaration `reserved` error instead of the confusing silent-hijack (the
import gate and the reserved-name gate are SEPARATE and BOTH apply). (2) a runtime `bind_import` SKIP
on BOTH engines (vm + interp) for `std.concurrency` member ∈ the four, so `import Shared from
std.concurrency` (which type-checks green but has no runtime module member) binds nothing instead of
faulting `module 'std.concurrency' has no member 'Shared'`. New tests RUN both engines (not just
check): whole-module construct+use of all four, and the from-import case that crashed the prior
attempt; plus reserved-still + per-name-licensing + len-3-does-not-license checker tests. Examples that
used the four bare now `import std.concurrency` (atomic/executor/executor_pool/executor_autodrain/
demo_executor/shared/rwshared/parallel_shared/parallel_cancel/ref_airlock/cancel_cpu + the two
concurrent_collection*). Docs (stdlib/syntax/concurrency) updated. `cargo test` (2708) + conformance +
clippy clean. (FINAL cleanup task — list/map/set→List/Map/Set — landed as task 5/5 above.)
**Checker polish (2026-06-25, follow-up to 4/5):** (a) a BARE (no `[T]`) `Shared`/`RwShared`/`Atomic`
annotation now hits a dedicated `resolve_type` arm instead of falling to the catch-all — unlicensed →
the SAME `unknown type '…' (import it from std.concurrency: …)` hint the `Shared[T]` arm gives;
licensed → the missing-type-arg error `type '…' expects 1 type argument(s), got 0` (matches the
user-generic struct/enum/newtype precedent). Mirrors the bare `Executor` arm. (b) the
`current_module_is_stdlib` stamp at `check_program` now calls the canonical `LoadedModule::is_std()`
(resolver) instead of an inline `dotted.first()==Some("std")` half-reimplementation that dropped the
`native.is_some()` clause — behavior-preserving (native std modules carry no concurrency annotations),
de-dups to ONE definition. Checker-only → three-engine parity by construction. New failing-then-green
tests: bare-without-import → hint; bare-with-import → missing-type-arg.
**Checker fix (2026-06-25, follow-up to 8fcbb3c — reserved-name-as-type-param hijack):** commit
8fcbb3c established the rule "a user generic type param named like a reserved/builtin type resolves as
the type param, not the builtin" but only patched the `Shared`/`RwShared`/`Atomic` arm in `resolve_type`
with an inline `if !self.type_params.contains_key(n)` guard. Five OTHER reserved-name arms still
preceded the `type_params` fallthrough and short-circuited it: `Socket`/`Listener`/`owned_str` silently
hijacked a same-named type param to the builtin (→ later type-mismatch), and the license-gated
`Executor`/`ptr` arms emitted a bogus `unknown type '…' (import …)`. Fix: HOISTED the `_ if
self.type_params.contains_key(n) => Ty::Param(n.clone())` arm to sit just below the scalar-primitive
literals (`int`/`float`/`bool`/`str`/`bytes`/`bytearray`/`nil`) and ABOVE every reserved/module arm, so
an in-scope type param uniformly shadows them all (kept below the scalars so `fn id[int](x: int)` still
resolves `x` to `int`, unchanged). The now-redundant inline guard on the `Shared`/`RwShared`/`Atomic`
arm was removed (one source of truth). Checker-only name resolution — runtime ctor/opcode dispatch
untouched, three-engine parity by construction. `is_reserved_type`/declaration-site reservedness
unchanged (`struct Executor` still reserved; `struct Socket` still allowed). New tests: extended
`type_param_named_like_concurrency_type_not_shadowed` to all five names, new
`bare_reserved_type_without_typeparam_still_errors` (negative cases preserved), new RUN parity test
`type_param_named_like_reserved_runs_both_engines` (check_graph + cooperative VM + OS-thread engine +
interp all agree).

**✅ global-namespace cleanup — task 2/5: FFI `ptr` gated behind `import std.ffi` (2026-06-25).** The
opaque C-ABI `ptr` type is no longer a global builtin — it now requires an import, **consistent with
the fixed-width integer types `int8`..`uint64`**. The `"ptr"` arm in `resolve_type` (checker) is gated:
it resolves to `Ty::Ptr` only if the module imported it (`imported_ffi_types`) or via a licensed alias
body, else `unknown type 'ptr' (import it from std.ffi: \`import std.ffi\`)`. Gating fires for ordinary
annotations AND `extern` param/return signatures (both go through `resolve_type`). Licensing: `ptr` is
added to `native_module_sig("std.ffi").types`; whole-module `import std.ffi` licenses `ptr` (keyed on
the exact `[std, ffi]` path — extern blocks use `ptr` pervasively, so whole-module licensing is the
default, UNLIKE the per-name-only widths), and `import ptr from std.ffi` licenses it per-name; `import
ptr as P` is rejected (no rename — backends key off the literal surface name). The runtime from-import
member check (interp + VM) skips `ptr` like the width names (type-only import, no runtime value). The
ungated C-marshalling paths (`resolve_ctype_d`, `resolve_ty_ro_d`) are untouched. `examples/ffi_ptr.chz`
now imports `ptr`; docs (stdlib/syntax/spec) updated. New tests + VM↔interp parity green. (3 cleanup
tasks remain: Match/Response/ProcResult→modules, Shared/RwShared/Atomic/Executor→std.concurrency,
list/map/set→List/Map/Set.)

**✅ global-namespace cleanup — task 3/5: `Match`/`Response`/`ProcResult`→modules (2026-06-25).** The
three synthetic native-module structs (`Match`/`std.regex`, `Response`/`std.request`,
`ProcResult`/`std.process`) are no longer global-reserved type names — they are now MODULE-OWNED. Built
native-module struct-type export: `native_module_sig` now populates `sig.struct_defs` + `sig.types` for
the owning module (the SAME field lists as the layout seed), and the existing is_std whole-module +
`import Name from module` import paths flow those into `struct_names`/`bare_types`, so the BARE type name
(`m: Match` / `Match(...)`) and qualified `regex.Match(...)` resolve ONLY when the module is imported.
The layout stays globally present (`StructOrigin::Builtin`) so FIELD ACCESS on a native return
(`regex.find(...).text`) keeps working with **no import**; the unconditional `struct_names` (bare-name)
reservation in `seed_stdlib_structs` is dropped. The hoist's already-defined gate now exempts a
`Builtin`-origin seed, so a user `struct Response` (without `import std.request`) shadows the seed and is
their own `User`-origin type. The names are now user-constructible once imported, so the compiler + interp
register the synthetic struct under its bare name in `module_types` (+ the interp seeds the `StructDef`)
to lower the ctor identically (VM↔interp parity). Unknown-type errors hint the owning module
(`types_by_name`). New checker + VM↔interp parity tests; docs (stdlib/syntax/spec) updated. (2 cleanup
tasks remain: Shared/RwShared/Atomic/Executor→std.concurrency, list/map/set→List/Map/Set.)

**✅ global-namespace cleanup — task 1/5: free `len()` dropped (2026-06-25).** The free `len(x)`
builtin is removed from all four stages (checker `is_reserved_name` + free-len arm, compiler
`is_builtin`, interp `builtins::is_builtin`/dispatch/`fn len`, VM dispatch + `fn builtin_len`); `len(x)`
now resolves as a plain `unknown name 'len'`, and `len` is no longer reserved (a user may declare
`fn len`). The `.len()` METHOD is kept everywhere (str/list/map/set/bytearray/Channel) and **added to
`bytes`** (checker `bytes_method_sig` + VM `bytes_method` + interp bytes-method arm, byte count,
VM↔interp parity). All free-len call sites in `examples/` migrated to `.len()`; docs (stdlib/syntax/
spec) updated. (4 more namespace-cleanup tasks queued: ptr→std.ffi, Match/Response/ProcResult→modules,
Shared/RwShared/Atomic/Executor→std.concurrency, list/map/set→List/Map/Set.)

**✅ runtime — `RwShared[T]`: the cross-task read-write box (2026-06-24).** New VM-core primitive
pairing with `Shared[T]`: **MANY concurrent readers OR one exclusive writer** (`RwSharedCore` wraps
`std::sync::RwLock<WireValue>` exactly where `SharedCore` wraps `Mutex`). Constructed value-first
(`RwShared(v)`, `T` inferred). Methods: `get() -> T` (shared read guard, snapshot), `set(x) -> nil`
(exclusive write guard, replace), `read(f: fn(T) -> R) -> R` (**shared** read guard — runs `f` against
the current value and returns its result, R-polymorphic in the closure's return, **no** write-back;
many `read`s run concurrently), `write(f: fn(T) -> T) -> nil` (**exclusive** write guard — `Shared.update`
under the write lock). Mirrored `Shared` end-to-end across BOTH engines: `Op::NewRwShared`,
`Obj::RwShared`/`WireValue::RwShared` (crosses the airlock as a SHARED `Arc` handle, NOT deep-copied —
the spawn/Channel airlock + GC trace + `to_wire`/`from_wire` twins), `Ty::RwShared` (sendable, new
reserved name), checker `rwshared_method_sig` + the `read` R-polymorphism recovered at the dispatch
seam, interp `Value::RwShared` + `eval_rwshared_method`. **`write`'s RMW is atomic across threads** via
a separate `update_lock` held for the whole write under `--parallel` (the `RwLock` write guard alone is
NOT enough — it's dropped across the user closure, so two writers could otherwise lose an update; same
discipline as `Shared.update`). Reentrancy limit (documented, mirrors `Shared.update`): a closure that
re-acquires the **same** box's write lock deadlocks. Golden `examples/rwshared.chz` (N tasks each
`write` a distinct key into one `RwShared[map]`, join, parent `read`s — order-independent →
byte-identical on VM/`--serial`/`--parallel`/interp). Docs: `docs/concurrency.md` §6c, `docs/stdlib.md`
§3, `docs/spec.md`/`docs/syntax.md` reserved-name + sendable enumerations. 2618+ tests + conformance
green, clippy clean.
**✅ stdlib — `std.concurrency.collection`: thread-safe collections over `RwShared` (2026-06-24).**
The capstone of the concurrency-collections work: pure-Chezzi ergonomic wrappers over the just-landed
`RwShared[Map[...]]` primitive, in the **first nested std module** (`std/concurrency/collection.chz` —
the dotted path resolves generically, no resolver special-casing). Two generic structs:
**`ConcurrentMap[K: Hashable, V]`** (`get`/`contains`/`len`/`snapshot` concurrent reads; `set`/`remove`/
`get_or_insert` exclusive writes — `get_or_insert` is COMPOUND-ATOMIC, check-and-insert in one write
lock) and **`ConcurrentCounter[K: Hashable]`** (`count`/`total` concurrent reads; `increment`/`add`
exclusive writes doing their read-modify-write in ONE closure → N tasks incrementing one key total
EXACTLY N, the classic race-free counter). Proven by live probe before building: (1) the nested path
resolves, (2) a struct whose only field is an `RwShared` crosses the spawn/`parallel:` airlock as a
SHARED `Arc` handle (NOT a deep copy) — 100 spawned `.increment` + 1 pre-bind → parent reads 101 on
VM/`--serial`/`--parallel`, (3) the single-write-lock RMW is race-free (exact-100 on `--parallel`,
5/5 deterministic). Construction is direct (`ConcurrentMap(RwShared({}))` — no `new_*` factory, since
turbofish can't bind `K`/`V`; same as `Counter({})`). Pure-Chezzi → 3-engine parity automatic; only
Rust touched is the two golden-test registrations (no engine code). Golden
`examples/concurrent_collection.chz` (deterministic: 100-task counter race → exactly 100, each-own-key
map → 285) byte-identical on VM/`--serial`/`--parallel`/interp. Tests: `examples/concurrent_collection_test.chz`
(6 `test fn`s incl. the airlock-sharing crux guard + `counter_race_exact`), VM
`golden_concurrent_collection_via_run_file` + interp twin. Docs: `docs/stdlib.md` §5 new
`### std.concurrency.collection`, `docs/concurrency.md` §6f pointer, `gaps.md` resolved. Resolves the
concurrent-collections / data-structures-concurrency gap (queue = `Channel`, atomic scalar = `Atomic`;
no `ConcurrentList`/`Set`/`Queue`). Full suite + conformance + clippy clean.
**✅ fix — FFI callback SIGSEGV (dangling `Cif`) (2026-06-24).** `chezzi run examples/ffi_qsort.chz`
segfaulted (libffi `classify_argument`, reachable via the qsort comparator callback) — a use-after-move:
`ffi_prep_closure_loc` stores a raw pointer to the callback `Cif`'s inner `ffi_cif`, but the `Cif` was
held **by value** in `CallbackClosure` (`src/native/cffi.rs`) and then moved into the
`callback_closures` `Vec`, relocating the `ffi_cif` and dangling that pointer. Layout-dependent, so the
3-engine `ffi_qsort` goldens (cooperative VM + interp + M:N `--parallel`) all passed while the CLI binary
crashed deterministically. Fix: `Box` the `Cif` (`_cif: Box<Cif>`) so its address is pinned across the
moves — exactly what the sibling `ctx: Box<TrampolineCtx>` already does. Regression guard:
`native::cffi::tests::boxed_callback_cif_address_is_stable_across_moves` (a compile-time check that the
field still derefs to `Cif` + the address-stability property). Full suite + conformance + clippy clean.

**✅ stdlib — `std.request` nit closed: per-call timeout + query builder (gaps.md "std.request nit") (2026-06-24).**
Two small independent additions. (A) **Per-call timeout override:** `std.request`'s `get`/`post`/`request`
now take an OPTIONAL trailing `timeout_ms: int` (mirrors the `std.net` `Socket.read(.., timeout_ms?)`
idiom) — a positive value applies ureq's per-request `.timeout(Duration)` (a TOTAL deadline overriding
the agent's hardcoded connect/read/write caps for that one call); `<= 0`/omitted falls back to the
defaults. A timeout surfaces through the existing `Error::Transport → Err` path (recoverable, never a
panic). New `expect_args_range(h, name, min, max)` helper in `src/native/mod.rs` (runtime mirror of
`FnSig::optional_tail`); `read_timeout` reads the guarded optional int. The checker's module-member
call path (`infer_method_call` `Ty::Module` arm) + the from-imported bare-fn path now route through
`check_args_range_w(.., min_params, .., widen=true)` so optional-tail arity is honored uniformly for
every native module fn (behavior-preserving — plain sigs have `min_params == params.len()`). std.request
`get`/`post`/`request` sigs → `optional_tail(.. + [Int], .., 1)` (installed post-match in
`native_module_sig` since the `func` closure borrows `sig`). The offload seam needs ZERO change (the
optional int crosses the airlock via `extract_native_args` generically → 3-engine parity by construction).
NO network golden for the timeout (non-deterministic); plumbing is asserted by a `do_get(.., Some(Duration))`
unit smoke + checker arity tests. (B) **Query builder:** `std.encoding.query_encode(params: Map[str,str]) -> str`
builds a `k=v&k2=v2` query string — both key and value percent-encoded (factored a shared `percent_encode`
helper reused by `url_encode`, no duplicated escaper), **keys sorted by RAW value** for a deterministic
golden, empty map → `""`. Lives in `std.encoding` (NOT `std.request`) because a native module name shadows
a same-named `std/<name>.chz` (the rand-task lesson) — no clean place for a pure-Chezzi request helper.
Pure CPU → NOT `is_blocking`. Golden `examples/encoding.chz` extended (sorted-key + empty + URL-compose
cases), 3-engine parity verified. Docs: `docs/stdlib.md` (§std.request timeout note + §std.encoding
query_encode), `gaps.md` (std.request nit struck → ✅ resolved). 2602 tests + conformance green, clippy clean.

**✅ stdlib — `std.collections` pure-Chezzi generic data structures (gaps.md "data structures
(heap/PQ, deque, counter, ordered map)") (2026-06-24).** New pure-Chezzi module `std/collections.chz`
(no native Rust, no seam — like `std/datetime.chz`/`std/path.chz`): three generic structs over `T`
built on the builtin `list`/`map`, so identical across all three engines. **`Heap[T]`** — binary
heap over a backing `List[T]` with a comparator **closure field** `less: fn(T,T)->bool` (verified a
generic struct can hold + call a fn-typed field); contract `less(a,b)==true ⇒ a pops first`, so
`a<b`=min-heap, `a>b`=max-heap (any `T`, no `Comparable` needed); `min_heap()`/`max_heap()` int
factories, `from_list(xs, less)` heapify (push-loop O(n log n)); push/pop O(log n), peek/len/is_empty
O(1). **`Deque[T]`** — **two-stack** amortized-O(1) both ends (front/back lists, drain-far-on-empty);
construct `Deque([], [])` (no `deque()` factory — a no-arg generic factory can't bind `T`).
**`Counter[T: Hashable]`** — `Map[T,int]` frequency table; `add`/`add_n`/`count` (0 if absent)/`total`/
`most_common(k)` (top-k by descending count, **stable insertion-order tie-break** via `map.keys()`
order + stable `sort_by`); construct `Counter({})`. **Empty semantics:** every removal/peek returns
`Option[T]` (`None`, never a fault — matches `list.pop()`). **Ordered map intentionally omitted** —
builtin `map` is already insertion-ordered (documented note only). TDD: `examples/collections_test.chz`
(12 `test fn`s — heap min/max/reverse/empty/from_list, deque fifo/lifo/both-ends/interleaved/empty,
counter counts/total/most_common+ties+k-clamp) RED→GREEN; golden `examples/collections.chz` +
`.expected` + `#[test] golden_collections_via_run_file` (VM==interp via `assert_file_parity`),
3-engine parity spot-checked. Docs: `docs/stdlib.md` (new `### std.collections` in §5), `gaps.md`
(data-structures struck → ✅ landed; ordered-map note). cargo test + conformance green, clippy clean.

**✅ stdlib — `std.datetime` pure-Chezzi civil-calendar date/time (gaps.md "duration/date
decomposition") (2026-06-24).** New pure-Chezzi module `std/datetime.chz` (no native Rust, no seam —
like `std/path.chz`) layered on the native `std.time` clock (`time.now()` only); everything else is
pure integer math (Howard Hinnant's branch-free civil-calendar algorithms). Surface: a `DateTime`
struct (`year`/`month`/`day`/`hour`/`minute`/`second`/`weekday`), `from_epoch`/`to_epoch` (round-trip
`to_epoch(from_epoch(e))==e`), `now`, `days_from_civil`/`civil_from_days` (a `(int,int,int)` tuple),
`is_leap_year`/`days_in_month`, `weekday`/`weekday_name`, fixed formatters `to_iso8601`/
`to_date_string`/`to_time_string`/`to_string`, and epoch-int duration helpers `add_seconds`/`add_days`/
`diff_seconds`/`diff_days`. **Contractual semantics** (in `docs/stdlib.md §5`): **UTC-only** (timezones/
DST/tz-database explicitly deferred); **weekday Sunday=0..Saturday=6** (matches native `std.time`:
epoch 0 == 1970-01-01 is Thursday == wd 4, differs from Python's Monday=0); **negative epochs floored**
(Chezzi `/`/`%` truncate toward zero, so internal `fdiv`/`fmod` floor-div helpers split the day/seconds
— `from_epoch(-1)`→1969-12-31 23:59:59 Wed, round-trips). Verified vectors: epoch 0, 1700000000 →
2023-11-14 22:13:20, `days_from_civil(2024,2,29)`==19782, leap 2000/2024, non-leap 1900/2023.
Pure-Chezzi → 3-engine parity automatic; still added `examples/datetime_test.chz` (9 `test fn` TDD
table) + golden `examples/datetime.chz`/`.expected` wired into `golden_datetime_via_run_file` (VM,
`assert_file_parity`) + `golden_datetime_chz` (interp twin). Docs: `docs/stdlib.md` (new `### std.datetime`
in §5), `gaps.md` (duration/date struck from the dogfood list — was falsely listed as landed). Full
suite + conformance + `clippy --all-targets -D warnings` clean.

**✅ stdlib — `std.path` pure-Chezzi path-STRING ops (gaps.md "path ops") (2026-06-24).** New
pure-Chezzi module `std/path.chz` (no native Rust, no seam — like `std/str.chz`/`std/iter.chz`) for
**unix `/` path-STRING manipulation, NOT filesystem I/O** (that stays `std.fs`). Built on the core
`str` methods (`split`/`starts_with`/`ends_with`) + the `str` `join` receiver. Surface:
`is_abs`/`is_rel`, `basename`/`dirname`/`split` (a `(str, str)` tuple = `(dirname, basename)`),
`ext`/`stem`/`with_ext`, `normalize`, `join`. Edge-case semantics match Python `os.path` (basename/
dirname/splitext) and Go `path.Clean`/`path.Join` for `normalize`/`join` (chose Go's simple join, NOT
Python's absolute-resets-earlier footgun) — every case is contractual in `docs/stdlib.md §5` (the
hard ones: `basename("a/b/")`→`""`, `dirname("/a")`→`"/"`, `ext(".bashrc")`→`""`, `ext("dir.d/file")`
→`""`, `normalize("/a/../../b")`→`"/b"`, `normalize("a/../../b")`→`"../b"`, `normalize("")`→`"."`).
Separator policy: `/` only, no Windows `\`. Pure-Chezzi → 3-engine parity is automatic (same `.chz`
on all engines); still added `examples/path_test.chz` (9 `test fn` TDD table, `cargo run -- test`) +
golden `examples/path.chz`/`.expected` wired into `golden_path_via_run_file` (`assert_file_parity` =
VM == interp). Docs: `docs/stdlib.md` (new `### std.path` in §5), `gaps.md` (path ops struck from the
pure-Chezzi dogfood list). Full suite + conformance + `clippy --all-targets -D warnings` clean.

**✅ stdlib — `std.process` polish (gaps.md "std.process polish") (2026-06-24).** `std.process` had
only `cmd(line)` via `sh -c` (injection-prone, stdout discarded on a non-zero exit). Added two
structured forms in `src/native/process.rs`: `run(line) -> Result[ProcResult]` (still `sh -c`, same
shell semantics as `cmd`) and `run_args(prog, args: List[str]) -> Result[ProcResult]` (runs the
program **directly, no shell** → arguments are passed literally, **injection-safe**). The new synthetic
struct `ProcResult { stdout: str, stderr: str, code: int }` carries **both streams + the exit code**: a
non-zero exit is a normal `Ok(ProcResult)` with `code != 0` (stdout NOT discarded), **only a spawn
failure** (no such program / permission) is `Err`; a signal-killed process reports `code = -1`. `cmd`
is unchanged (back-compat — `examples/sys.chz` still green). The `List[str]` argv crosses the off-heap
offload boundary via a NEW seam variant `NativeArg::List(Vec<String>)` + `Host::arg_str_list` (default-
err), implemented on all three hosts (`VmHost` reads the live heap list, `extract_native_args`
snapshots it to `NativeArg::List`, `OffloadHost` serves it back off-thread, `InterpHost` reads the live
list) — a direct clone of the existing `Map[str,str]` triad, so 3-engine parity (interp == cooperative
VM == M:N) holds by construction at the NativeFn seam. `run`/`run_args` wired into `is_blocking()`
(subprocess I/O → offloaded under the OS-thread engine). `ProcResult` is registered with the other
synthetic stdlib structs in the compiler (`src/compiler/mod.rs`, declaration-order field names) and
seeded in the checker (`seed_stdlib_structs` + `native_module_sig` std.process arm). Golden (VM ==
interp via `assert_file_parity`, byte-identical under run/--serial/--parallel):
`examples/process_polish.chz` — proves nonzero-is-Ok-with-code, the `$(...)`/`;`/`&&` injection-safety
of `run_args`, and the spawn-failure `Err` path. Docs: `docs/stdlib.md` (§std.process extended +
`ProcResult` reserved), `gaps.md` (std.process polish → ✅ RESOLVED). **Deferred:** stdin piping,
output streaming, per-process env/cwd overrides. Full suite + conformance + `clippy --all-targets -D
warnings` clean.

**✅ stdlib — encoding/crypto/uuid native modules (gaps.md "Encoding/crypto") (2026-06-24).** Three
new native modules, all hand-rolled with **zero new crates** (repo dependency-free policy):
`std.encoding` (`src/native/encoding.rs`) — base64 std + URL-safe (RFC 4648), hex, RFC 3986 URL
percent-encode/decode; `std.crypto` (`src/native/crypto.rs`) — `sha256` (FIPS 180-4) + `md5` (RFC 1321),
both validated against published test vectors + cross-checked vs `sha256sum`/`md5sum`; `std.uuid`
(`src/native/uuid.rs`) — `v4` (random, RFC 4122) + `uuid_seed` (deterministic), with its OWN
process-global SplitMix64 stream that reuses `rand::next_u64` (the RNG step is not duplicated) and
auto-seeds from OS entropy. The native seam carries only `str`, so every fn is `str`-in and
`str`/`Result[str]`-out: encoders/digests are infallible `str`; base64/hex/url `decode` UTF-8-validate
their output and surface malformed input OR non-UTF-8 bytes as a catchable `Err` (never a panic). All
members are pure CPU transforms → NOT in `is_blocking()` (run inline on every engine), giving 3-engine
parity (interp == cooperative VM == M:N) by construction at the NativeFn seam. Wiring mirrors std.rand/
std.fs: `MEMBERS` table per file, `src/native/mod.rs` (`pub mod` + `native_name`/`native_members` arms +
the uniqueness/non-blocking test lists — `uuid` reseed is named `uuid_seed`, not `seed`, to keep bare
member names unique since `std.rand` owns `seed`), `src/checker/mod.rs` `native_module_sig` arms.
Goldens (VM == interp via `assert_file_parity`): `examples/encoding.chz` / `crypto.chz` (deterministic
round-trips + digests) and `examples/uuid_shape.chz` (`uuid_seed`-deterministic stream + shape check,
serialized on `TEST_UUID_LOCK`). Docs: `docs/stdlib.md` (new §std.encoding/§std.crypto/§std.uuid),
`gaps.md` (Encoding/crypto → ✅ RESOLVED). **Deferred:** the str-only seam can't return raw bytes, so
binary round-trip (image → bytes) needs a bytes-arg/return seam expansion; `sha512`/`sha1`/`uuid-v7`
not added. Full suite + conformance + `clippy --all-targets -D warnings` clean.

**✅ stdlib — `std.fs` filesystem mutations (gaps.md "fs mutations") (2026-06-24).** `std.fs` was
read-only; it now writes. Six new natives in `src/native/fs.rs`, each mirroring `std.io.write_file`'s
fault idiom (`Ok(NativeRet::Ok(Nil))` / `Ok(NativeRet::Err("{path}: {e}"))`) so an I/O failure is a
catchable `Err`, never a panic — and all are `Result[nil]`: `mkdir(path)` (recursive via
`create_dir_all`, mkdir -p, idempotent on an existing dir), `remove_file(path)`, `remove_dir(path)`
(**empty-only / non-recursive** — faults on a non-empty dir, no silent `rm -rf`), `rename(from, to)`,
`copy(from, to)` (file contents; byte count dropped for `Result[nil]` parity with `write_file`),
`append(path, contents)` (`OpenOptions` create+append — creates if absent, **never truncates**,
complementing `write_file`'s overwrite). 3-engine parity is by construction at the NativeFn seam (interp
/ cooperative VM / M:N all call the same `fs.rs` fn). Wired into `is_blocking()` (std.fs arm) so the M:N
engine offloads them like the read ops; checker `native_module_sig` std.fs arm gains the six sigs
(`mkdir`/`remove_file`/`remove_dir`: `str -> Result[nil]`; `rename`/`copy`/`append`: `str, str ->
Result[nil]`). **Limit (documented, deferred):** recursive dir removal (`rm -rf`) is intentionally not
provided — `remove_dir` is empty-only to avoid an accidental wipe. Tests (RED-first): 2 `fs.rs` unit
(roundtrip mkdir→append→rename→copy→remove + recoverable-error cases via a temp-dir `Host` mock), the
`is_blocking` offloadable-set + uniqueness-guard lists, 2 checker tests (the six sigs typecheck as
`Result[nil]`; wrong-arity rejected), and the self-cleaning golden `examples/fs_mutations.chz`
(VM + interp twins, serialized via `FS_SCRATCH_LOCK` on the shared `examples/.fs_scratch`; gitignored;
fixed status lines + read-back contents, no absolute paths) — manually verified byte-identical under
run / --serial / --parallel and leaves no scratch behind. No grammar change (plain import + member
calls; conformance clean). Docs: `docs/stdlib.md` (§std.fs split into Queries/Mutations + the
non-recursive/never-truncate limits), `gaps.md` (fs mutations → ✅ RESOLVED). Full suite + conformance +
`clippy --all-targets -D warnings` clean.

**✅ stdlib — `std.rand` native RNG (gaps.md highest stdlib gap) (2026-06-23).** A SplitMix64 PRNG.
**Native module `std.rand`** (`src/native/rand.rs`) exposes scalars only: `seed(n: int) -> nil`
(deterministic reseed), `float() -> float` in `[0, 1)`, `int(lo, hi) -> int` (half-open `[lo, hi)`;
faults `rand.int(lo, hi): hi must be > lo` if `hi <= lo`, unbiased via rejection sampling), `bool()`.
State is a single **process-global** `OnceLock<Mutex<u64>>` (NOT thread-local / NOT Host-side), so all
three engines (interp / cooperative VM / M:N `--parallel`) share one stream at the NativeFn seam →
any *sequential* draw sequence is byte-identical across engines (3-engine parity by construction).
Auto-seeds from OS entropy (`libc::getrandom` on Linux, with a time/address/counter SplitMix64-mix
fallback) on first use; `seed(n)` makes it deterministic. Draws are inline CPU → **not** in
`is_blocking()`. **Generic helpers in `std.iter`** (pure Chezzi, call native `rand.int`): `shuffle[T]`
(new Fisher–Yates permutation, non-mutating), `choice[T] -> Option[T]` (`None` on empty), `sample[T]`
(`k` without replacement, `k` clamped to len). The split is **forced**: the native seam carries only
engine-neutral scalars (cannot return a generic `List[T]`), and a native module name short-circuits a
same-named `std/<name>.chz` in the resolver — so scalars + helpers cannot co-inhabit a `rand`
namespace. **Limit (documented, not a bug):** under `--parallel`, *concurrent* draws from multiple
tasks interleave nondeterministically on the shared global RNG (engines may diverge) — the goldens draw
strictly sequentially to stay deterministic on all three engines; this is the same class as the existing
cooperative-vs-MN timing escape hatches. Tests (RED-first): 5 `rand.rs` unit (SplitMix64 golden vector
in isolation, float/int/bool range + half-open + empty-range fault + auto-seed shape), native wiring +
non-blocking + uniqueness lists, and 3 run-file goldens (`rand_seeded` all-four-fns seeded,
`rand_shape` unseeded range-only "ok" lines, `rand_iter` shuffle/choice/sample) run as ONE serialized
test (shared global RNG) + `assert_file_parity` (VM == interp); manually verified VM == `--serial` ==
`--parallel` byte-identical on the seeded goldens. No grammar change (plain import + member calls;
conformance clean). Docs: `docs/stdlib.md` (new §std.rand + std.iter shuffle/choice/sample),
`gaps.md` (std.rand → ✅ RESOLVED). Full suite + conformance + `clippy --all-targets -D warnings` clean.

**✅ DX — print `sep=`/`end=` + assert message format (gaps.md DX gaps #5 + #6) (2026-06-23).** Two
cohesive builtin-ergonomics fixes. **print (#5):** `print` is now special-cased to accept exactly two
named arguments — `sep` (default `" "`, joins the positional args) and `end` (default `"\n"`, appended
after). Both must be `str` and may be runtime expressions (not just literals). `print("a","b")` → `a b\n`
(unchanged), `print("a", end="")` → `a` (no newline → incremental output), `print("a","b", sep="-",
end="!")` → `a-b!`. Wired through **desugar** (`print` keeps only `sep`/`end` on its Call un-rewritten,
rejecting any other kwarg / a dup with "print() only accepts the named arguments 'sep' and 'end'"),
**checker** (each `sep`/`end` value must be `str`, else "print() sep/end must be str, found <T>"),
**compiler** (new `Op::CallPrintSep{argc}` that pushes `sep`+`end` after the args; a plain `print(...)`
with no kwargs still emits `Op::CallPrint` → output byte-identical to before), and **both engines**
(`vm::do_print_sep` + the interp print branch, same join-with-`sep`/append-`end` order: positional args →
sep → end). **assert (#6):** the `assert cond, "msg"` STATEMENT form already existed end-to-end; the fix
is the **fault wording** — a failing `assert false, "boom"` now faults as `assertion failed: boom` (was
the raw `boom`), bare `assert false` keeps exactly `assertion failed`, and `msg` is still evaluated lazily
on the failing path only. Two fault sites (`vm/mod.rs` `Op::Assert` + `interp/mod.rs` `Assert`),
byte-identical across engines. Tests (all RED-first): 4 desugar (sep/end kept, unknown/dup kwarg rejected),
3 checker (sep/end str ok, sep/end non-str rejected), 7 VM behavior (end="", sep=, both, default unchanged,
runtime expr, only-end), 1 VM↔interp print parity (8 forms), updated assert tests + new lazy-on-pass guards
on both engines, and golden `examples/print_kwargs.chz` (VM == interp == `.expected`). Docs:
`docs/syntax.md` (assert fault wording + lazy msg), `docs/stdlib.md` (print signature with `sep=`/`end=`),
`gaps.md` (gaps #5/#6 → RESOLVED log). No grammar change (print kwargs are ordinary call named-args;
conformance clean). Full suite + conformance + `clippy --all-targets -D warnings` clean.

**✅ DX — stepped / reverse range (gaps.md DX gap #4) (2026-06-23).** `range()` gained a 3-arg
`range(start, end, step)` form (the 1-arg/2-arg forms are byte-unchanged). `step` is a **non-zero int**:
positive counts up half-open `[start, end)`, negative counts down half-open (excludes `end`), e.g.
`range(10, 0, -1)` → `[10, 9, …, 1]`, `range(0, 10, 2)` → `[0, 2, 4, 6, 8]`. A wrong-direction step or
`start == end` → `[]`; `step == 0` raises a recoverable fault `range() step cannot be zero`. All the
element-count / cap math runs in **i128** so a huge span or an `i64::MIN` bound/step can't overflow or
panic (`i64::MIN.abs()` would); the 10M result cap is unchanged. The materialization is a single shared
`slice::range_values(start, end, step) -> Result<Vec<i64>, String>` called by **both** engines (interp
`builtins::range` + VM `builtin_range`) so the values and fault text are byte-identical. **SECONDARY
(landed): a range literal is now sliceable** like a list — `(0..10)[::2]` → `[0, 2, 4, 6, 8]`,
`(0..5)[::-1]` → `[4, 3, 2, 1, 0]` — by materializing the (ascending, step-1) range via the `range`
builtin then reusing the **existing** `Op::GetSlice` / `slice::slice_indices` `::step` machinery (compiler
Slice arm emits `CallBuiltin("range", 2)` when the obj is a `Range`; interp `eval_slice` mirrors it). A
bare range still has no value anywhere else (`x := 0..10` keeps its compile error). **Decision: `a..b`
stays ascending — no auto-reverse** (`for i in 10..0` yields nothing, the lazy for-loop path is
untouched); the down-count idiom is `range(start, end, -1)`. No grammar change (the `..` syntax is
untouched; conformance clean). **Parity by construction** (shared helper). Tests (all RED-first): 3
`slice::range_values` unit tests (up/down/by-N, empty + zero-step, overflow/INT_MIN edges) + interp +
VM runtime tests (up/down/step-zero/empty/range-slice) + 2 checker tests (1/2/3-arg accept, 0/>3 reject,
non-int reject; range-slice infers `List[int]`) + golden `examples/range_step.chz` (VM == interp ==
`.expected`). Docs: `docs/syntax.md` (range section + slicing note), `docs/stdlib.md` (range signature),
`gaps.md` (gap #4 → RESOLVED log, open DX items renumbered 1..3). Full suite + conformance +
`clippy --all-targets -D warnings` clean.

**✅ DX — collection operators (gaps.md DX gap #3) (2026-06-23).** List `+` (concat) / `*` (repeat)
and set `| & - ^` (union / intersection / difference / symmetric-difference) now work as operators,
behaviour **identical to the existing methods** (`.concat`, `.union`/`.intersection`/`.difference`;
`^` symmetric-difference has no method form). Implemented as **runtime-opcode dispatch** (NOT compiler
desugar — the compiler has no operand type info): new value-typed match arms in `vm::arith` +
`vm::bitwise` (a shared `Vm::set_op` + `Vm::list_repeat`), mirrored byte-for-byte in
`interp::eval_binary` (free-fn `set_op`/`list_repeat`), plus the type arms in checker `infer_binary`
(list/set element types must match — a mismatch is the existing `cannot apply …`/`bitwise operator …
requires int operands or two sets` error; `[] + [1]` infers `List[int]` via `merge_unknown`).
`list * int` is **commutative** (`3 * [0]` too, Python-style); `n <= 0` → `[]`; a giant `n` raises a
recoverable `list repeat capacity overflow`, never a process abort. The guard is two-layered: an
`isize::MAX` byte-size check (overflow-safe `checked_mul`) **plus** a `Vec::try_reserve_exact`
allocation-feasibility check — the latter catches huge-but-representable counts (~1e17..5.7e17 for a
1-element list) that pass the byte bound yet abort `Vec::with_capacity`; `str.repeat` carries the same
two-layered guard. Set results preserve insertion order (union = mine-then-other; intersection/difference =
mine-filtered; symmetric-difference = mine∉other then other∉mine) so both engines print identically.
Plain int bitwise + `<< >>` are unchanged (`<< >>` stay int-only). **Parity:** golden
`examples/collection_ops.chz` runs VM == interp == `.expected` (via `assert_file_parity`), confirmed on
`--serial` and `--parallel` too. Tests: 11 checker inference/rejection tests + VM eval-correctness +
list-repeat overflow recoverable-fault + the golden parity test (all RED-first). Docs:
`docs/syntax.md` §4 operator table + collection-operators note, `docs/stdlib.md` (list/set method
operator forms), `docs/grammar.bnf` (bitwise cascade note — same tokens, no grammar change; conformance
clean), `gaps.md` (gap #3 → RESOLVED log, open DX items renumbered 1..4). Full suite (2517) +
conformance + `clippy --all-targets -D warnings` clean.

**✅ SYNTAX — `else if` → `elif` (Python-style single keyword) (2026-07-09).** The two-token `else if`
chain is REPLACED by a single `elif` keyword (Python "one obvious way"; `else if` no longer parses — a
hard parse error). Pure front-end change: lexer (`Token::Elif` + `("elif", Token::Elif)` keyword +
reserved-words coverage), parser (`parse_if` statement + `parse_if_expr` expression forms key on
`Token::Elif` instead of `Else`+`If`), `docs/grammar.bnf` (`<elseIf> ::= "ELIF" …`, `<ifExprTail>` gains
the `"ELIF"` arm), conformance `symbol()` (`Token::Elif => "ELIF"`), editor tmLanguage regenerated. No
checker/compiler/VM edit → serial-VM == M:N-VM parity by construction (both consume the same AST from one
front-end). Migrated every `.chz` (examples/, std/, corpus) + embedded fixtures + docs. Tests: lexer
`lexes_elif_keyword`; parser `if_elif_else`/`elif_chain_three_branches`/`rejects_bare_else_if_stmt` +
`if_expr_elif_chain`/`if_expr_elif_still_requires_final_else`/`if_expr_rejects_bare_else_if`.

**✅ DX — chained `elif` in expression-`if` (gaps.md DX gap #2) (2026-06-23).** `a := if p: 1
elif q: 2 else: 3` parses without parentheses. Parser-only (~10 lines): `parse_if_expr`
(`src/parser/mod.rs`) branches after the `then` — if the next token is `Elif` (was `Else`+`If` before
the 2026-07-09 `elif` rename above) it captures the `elif` span and recurses into `parse_if_expr` for the
else-branch (right-associative nested `ExprKind::IfElse`), else the existing `else: <expr>` tail. Final
`else` stays mandatory (the recursion ends in its own `expect(Else)`). No checker/compiler/interp/VM change — the nested `IfElse` is the same
AST shape the hand-parenthesized workaround produced, so both engines already evaluate it byte-identically.
**Parity by construction.** Tests: 2 parser unit tests (chain nests right-associatively; chain still
requires final else) + golden `examples/expr_else_if.chz` (VM == interp == `.expected`). Docs:
`docs/grammar.bnf` (`<ifExpr>` + new `<ifExprTail>` tail rule), `docs/syntax.md` (chained example),
`gaps.md` (gap #2 → RESOLVED log, others renumbered). Full suite + conformance + `clippy --all-targets
-D warnings` clean.

**✅ Feature — FFI C-buffer alloc layer `std.ffi.alloc`/`alloc_zeroed`/`free` (feasibility-ladder
tier 3) (2026-06-22).** Allocate raw C-laid-out memory to hand to a C array/buffer API (`qsort`,
`bsearch`, `fread`-into-buffer): `alloc(nbytes) -> ptr` (`malloc`; garbage bytes),
`alloc_zeroed(nbytes) -> ptr` (`calloc`; zeroed), `free(p)` (`free`; returns nil). Fill/read with the
already-shipped `store_*`/`load_*` deref builtins — **no** bulk-copy helper (the loop idiom is the
surface). **Allocator:** direct `unsafe extern "C"` `malloc`/`calloc`/`free` (the **libc** allocator,
NOT Rust's `GlobalAlloc`), so a buffer may be handed to a C fn that reallocs/frees it and it pairs with
the same allocator `cffi`'s `owned_str` free path uses; extern decls resolve at link time, zero
per-call dlsym/libffi overhead. **Manual free** (`defer ffi.free(p)`) — a `ptr` is never auto-freed
(consistent with the FFI-ptr rule); forgetting **leaks**. **Faults (recoverable, never segfault/abort):**
`nbytes < 0` → `ffi.alloc: negative size`; `malloc`/`calloc` returning NULL for `nbytes > 0` →
`ffi.alloc: out of memory` (OOM checked only when `n > 0`, so a legitimate NULL from `malloc(0)` is not
mis-reported); `free(ffi.null())` is a **no-op** (does NOT route through `base_addr`); `nbytes == 0`
passes through (impl-defined). Double-free / use-after-free / OOB store_/load_ are the user's
responsibility (documented UB, no bounds/lifetime tracking — that's the deferred auto-buffer type).
`#[cfg(unix)]`-gated (non-unix registers the names but every call errors, mirroring the deref builtins).
**Parity by construction:** pure-additive on the engine-neutral `Host`/`NativeFn` seam — no VM/interp
edit — so VM == interp == M:N. **Wiring:** 3 new `MEMBERS` entries (now 59) in `src/native/ffi.rs` +
`native_module_sig`'s `std.ffi` arm (`src/checker/mod.rs`: `alloc`/`alloc_zeroed`:int→ptr,
`free`:ptr→nil). **Tests:** 5 ffi unit tests (roundtrip+free, zeroed-reads-zero, negative-size error,
free(null) no-op, MEMBERS coverage) + 1 checker sig test + 2 cffi two-engine parity tests (alloc+fill+
read+free; alloc_zeroed) + the **capstone `examples/ffi_qsort.chz`** golden on BOTH engines (sort a
Chezzi `int` list via libc `qsort` with a Chezzi `fn(ptr,ptr)->int` comparator that `load_int64`s both
sides — the marquee proof callbacks + deref + alloc all compose; also verified on `--parallel`). Full
suite + conformance + `clippy --all-targets -D warnings` clean. Docs: `docs/stdlib.md` (new alloc
surface + qsort idiom), `docs/ffi-and-packaging.md §1b` (tier 3 → LANDED; `qsort`/`bsearch` of a Chezzi
list now fully works; honest about what remains deferred: stored/cross-thread callbacks + variadics +
a GC-tracked owned-buffer), `docs/spec.md` + `docs/syntax.md` (FFI limits: manual C-buffer alloc now
available).

**✅ Feature — FFI memory-deref builtins `std.ffi.load_*`/`store_*` (feasibility-ladder tier 2)
(2026-06-22).** Read/write the **C-owned memory behind an opaque `ptr`** — for struct fields, return
buffers, event payloads, and C output-params a library hands you. Two-form API (fixed-arity native
fns, no variadic/optional machinery): a base form at byte offset `0` and an `_at(p, off)` byte-offset
form (the `_at` *store* takes the offset *before* the value). **Loads** (`-> int/float/bool/ptr/str`):
`load_int` (C `long`), `load_int8`..`load_int64` (sign-extend), `load_uint8`..`load_uint64`
(zero-extend), `load_float` (C `double`), `load_float32` (C `float`, widened), `load_bool`, `load_ptr`
(deref `void**`), `load_str` (copy a NUL-terminated C string, not freed). **Stores** (`-> nil`,
natural C width) mirror every width except `str` (`store_str` deferred — unbounded-write footgun).
**Reuse, not re-derive:** the loads/stores delegate to `cffi::read_field`/`write_field` (made
`pub(crate)`) — the *same* sign/zero-extend + truncation rules the callback/struct paths already use —
over a transient byte slice (`slice::from_raw_parts[_mut]`) at the natural width; `float32`/`str`
hand-roll (no f32 arm in `read_field`; `CStr::from_ptr` for the string). **Safety:** every fn rejects
a **NULL** base pointer with a *recoverable* `HostError` (`ffi.<fn>: null pointer`) **before** any
deref — the only cheaply-checkable guard; a dangling/misaligned/OOB *non-null* pointer is documented
UB (like `ctypes`). Mitigation `ctypes` lacks: a `ptr` is opaque and **cannot be forged from an int**
(provenance is C-sourced). Deref bodies are `#[cfg(unix)]`-gated (a non-unix build registers the names
but every call errors). **Parity by construction:** pure-additive on the engine-neutral `Host`/
`NativeFn` seam — no VM/interp edit — so VM == interp == M:N. **Wiring:** all 56 `std.ffi` members in
`MEMBERS` (`src/native/ffi.rs`) + `native_module_sig`'s `std.ffi` arm (`src/checker/mod.rs`).
**Tests:** 13 ffi unit tests (width/extend boundaries, `_at` offset, store→load round-trip, natural-
width store, NULL-error, MEMBERS coverage) + 3 checker sig tests + 3 cffi two/three-engine parity
tests (a `cc`-built `mkrec()` returning a `ptr` to `{int32 a@0; int64 b@8; double c@16}`, read/written
field-by-field). Full suite (2478) + conformance + `clippy --all-targets -D warnings` clean. Docs:
`docs/stdlib.md` (new `std.ffi` surface), `docs/ffi-and-packaging.md §1b` (tier 2 → LANDED; the
remaining gap at the time — `qsort`/`bsearch` of a Chezzi *list* needing a C-buffer alloc layer — has
**since landed**, see the tier-3 entry above), `docs/spec.md` (FFI v1 limits: `ptr` memory now
readable/writable), `docs/syntax.md`.

**✅ Feature — FFI sync scalar callbacks (callbacks #4, sync subset) (2026-06-22).** An `extern "lib":`
fn can now take a **function-typed parameter** spelled with the *existing* `fn(a, b) -> r` type (no new
grammar) whose params + return are all C scalars (`int`/`float`/`bool`/`ptr`/`int8`..`uint64`; no
`str`/struct/nested callback) — a Chezzi closure passed to C as a C function pointer that C calls
*back* synchronously, on the same thread, during the extern call. **Pipeline:** `CType::Callback{params,
ret}` + an `is_scalar()` helper (`src/native/cffi.rs`); the checker's `assert_marshallable` accepts a
scalar `Ty::Func` in **param** position only (a func-typed *return* is rejected) and `resolve_ctype_d`
lowers `Type::Func` → `CType::Callback`; `Cffi::call` builds a libffi `ffi_closure` trampoline (raw
`ffi_prep_closure_loc` + `low::closure_alloc`/`closure_free`) whose userdata holds a `*mut dyn Host` +
the arg index + the signature + a fault slot, pushes the trampoline's code address as the `void*` arg,
and frees the closure when `call` returns (**sync scope ⇒ no GC rooting**). **The one new engine seam**
is `Host::invoke_callback(arg_index, &[NativeRet]) -> NativeRet` (keyed by arg index so no engine
`Value` leaks across the FFI layer): the VM host re-enters via `guarded`+`invoke_value`; the interp
host gained a callback-capable `InterpCallbackHost` (holds `&mut Interp`, re-enters `call_value`) used
only by `call_cffi`. **Fault rule (stronger than ctypes):** the trampoline body is `catch_unwind`-
wrapped — a Chezzi fault or panic writes a zeroed C result (clean unwind), stashes the error, and
re-raises it as the extern call's own error (ctypes swallows to stderr + returns 0). **Tests:** a
`cc`-built `.so` fixture (`int apply(int,int(*)(int))` + a `double` variant) drives int/float
round-trips, fault + panic re-raise, and **two-engine + three-engine** (`--parallel`) parity (sync
callback fires on the calling worker thread — no cross-thread hand-off). 7 cffi tests + 6 checker tests
green; full suite (2459) + conformance + `clippy --all-targets -D warnings` clean. Docs: `docs/spec.md`,
`docs/syntax.md`, `docs/ffi-and-packaging.md §1b` (incl. the **feasibility ladder**: (1) sync scalar
done, (2) pointer-deref builtins → `qsort`/`bsearch`, (3) stored/cross-thread = own milestone, needs a
GC-rooting registry + thread-safe re-entry; **biggest caveat:** `--parallel` has **no GIL**, so
cross-thread is strictly harder than Python — needs a mini-GIL or thread-marshalling). `cc` added to
`[dev-dependencies]`.

**✅ Feature — one-way C-like `int`→`float` implicit widening (2026-06-22).** An `int` value now flows
into a `float` SLOT automatically, converted to a real `f64` (the reverse stays a lossy type error).
The design (Architecture C) emits a **real** runtime conversion at each value-DEFINITION boundary,
driven by the static annotation already in the AST — so it is byte-identical on the checked CLI path
AND the checker-bypassing parity harness (two-engine VM↔interp parity by construction; the M:N
`--parallel` engine shares the compiler so it is covered too). **Checker** (read-only): a scoped
`assignable_w(expected, actual, widen)` adds `(Float, Int) => true` only at compiler-coercible sinks
(typed binding, fn/method/closure args via `check_args_w`, returns, struct-field defaults, native/extern
float params) — the type-blind assign targets (`p.x = 3`, `xs[0] = 3`, `m[k] = 3`, tuple-target,
reassign-to-float-local) stay STRICT (no runtime hole); `infer_list`/`infer_map`-value unify an
int/float mix to `float` (one-way). **Compiler**: new cheap inline `Op::CoerceFloat` (mirrors `AsInt`,
reuses `n as f64`), emitted at typed binding, the float-param callee prologue (so an int *variable* widens
at the boundary, any caller), `-> float` returns (incl. inline-expr bodies), per-`float`-field struct
construction, and `float`-annotated / all-literal collection literals. **Interp** (frozen oracle, a
tree-walker — no bytecode): an equivalent `coerce_float`/`coerce_value_to_annotation` helper at the
SAME AST boundaries → parity. **Semantic proof:** `x: float = 3` makes `x / 2 == 1.5` (real float
division), not `1`. **Anti-lossy negatives stay type errors** (`y: int = 2.3`, `-> int: return 2.3`,
`float` into `List[int]`, `int`→`float` across a **newtype**, reassign-int-to-float-local). **SUPERSEDED (2026-07-13):** the "carve-outs" this entry
claimed were *documented, not holes* were in fact a **soundness hole** — the checker widened more than
the type-blind compiler could coerce, leaving a runtime `Int` under a static `float` at the param /
return / field / collection sinks (no annotation could fix them). Narrowed to Go's untyped-constant
rule; see the 2026-07-13 entry below. A plain reassign `x = 3` to a float local remains a strict
(rejected) target. Native `sqrt(16)` / extern `cos(2)` widening confirmed hole-free (host promotes).
Docs: `gaps.md` → RESOLVED log, `docs/syntax.md §3`, `docs/spec.md`, `docs/stdlib.md`.

**✅ Bug fix — `ref` shared-method-name dispatch no longer falsely rejects an EXPRESSION receiver
(2026-06-22).** When ≥2 structs share a method name with differing param ref-ness (the receiver type
disambiguates which signature applies, per `docs/syntax.md §3`), a call with a *named-local* receiver
(`a := A(0); a.apply(r)`) type-checked but the equivalent *inline-expression* receiver (`A(0).apply(r)`,
or `mk().apply(r)` where `fn mk() -> A`) was falsely rejected ("expected Ref[int], found int") — an
over-rejection of valid code (safe, not unsound). Root cause was **desugar-only, pre-type**:
`callee_param_is_ref` resolved the receiver's struct (to pick the right sibling's `ref`-ness) only for a
named-local `Ident`; an expression receiver fell through to the agreement-gated name table, which returned
`None` for disagreeing siblings, so the `ref` arg was wrongly auto-deref'd before the checker ran. Fix:
new `receiver_struct_ty` helper resolves the receiver struct name for a named local, an inline ctor call,
AND a struct-returning free fn (new `ModReg::fn_ret_struct` map from the declared return type), driving
`methods_by_struct` uniformly. Desugar runs once before every engine, so VM == interp == serial ==
parallel is structural (no `src/interp` edit). Tests: `lowers_ref_arg_through_ctor_receiver_typed_method`
/ `..._fn_call_receiver_typed_method` (desugar), `ref_through_shared_method_name_ctor_receiver_ok` /
`..._fn_receiver_ok` + `ref_shared_method_byval_sibling_ctor_receiver_ok` (checker), extended
`examples/ref_indirect.chz` golden (stdout `42`, two-engine parity). Negative guards intact (single-struct
mismatch + by-value-into-ref still error). Docs: `gaps.md` entry → RESOLVED.

**✅ Bug fix — a struct/enum method whose name collides with a built-in method (`add`, `map`, `push`,
`len`, … the `BUILTIN_METHODS` list) now gets named- and default-argument support (2026-06-27).**
Previously the desugar `is_builtin_method(name)` guard (two sites in `src/desugar/mod.rs`) skipped ALL
method resolution for any builtin-colliding name — because the receiver MIGHT be a List/Set/Map/str the
pre-type pass can't see — so `c.add(amount=5)` on a user `Counter` was rejected with the misleading
"named arguments are only supported on functions, struct constructors, and struct methods" (it IS a
method). Fix: on the builtin branch, resolve via the already-existing receiver-type-aware lookup
(`receiver_struct_ty(obj)` → `methods_by_struct[(sname,name)]`) BEFORE bailing — when the receiver's
struct/enum type is statically knowable pre-type (a typed local, an inline ctor call, or a
struct-returning fn call) and that struct defines the method, the user method's spec drives full
named/default rewriting; a genuine builtin receiver (or an unknowable one) still returns None and is
left untouched (no name-keyed fallback that could mis-bind a builtin). `normalize_call`'s `method_spec`
arm + `callee_param_is_ref` both updated; the diagnostic for the unknowable-receiver case is now accurate
("method '…' reuses a built-in method name; named/default arguments need a receiver whose struct type is
statically known — bind it to a typed local or pass positionally"). Desugar runs once before both engines
⇒ two-engine parity is structural (no `src/interp` edit). Tests: 7 new desugar unit tests
(`builtin_named_method_*`, `enum_builtin_named_method_annotated_receiver`, accurate-error +
no-struct-defines guards) + the `real_builtin_set_add_untouched` / `builtin_method_name_not_normalized` /
`ambiguous_method_named_errors` boundary guards stay green; new `examples/builtin_named_method.chz` golden
+ `golden_builtin_named_method_chz_matches_expected_and_interp` (VM == interp). Docs: `docs/syntax.md`
limitation sentence rewritten; the `BUILTIN_METHODS` doc-comment updated. Known boundary (pre-existing,
not introduced here): an inferred enum receiver `m := E.Variant` is Field-shaped so its type isn't
statically known → falls to the accurate diagnostic (annotate the local or pass positionally).

**✅ Soundness fix — two missing duplicate/collision checks in the checker are now rejected (both
checker-only; two-engine parity preserved by construction — rejected programs never reach an engine,
accepted programs are byte-identical).** (1) **Import name collisions.** `bind_import` recorded a value
member via `declare()`, a function member into a separate `self.functions` map, and a module into
`imported_modules`, with **no cross-namespace duplicate check** — so `import v from vmod` (value) +
`import v from fmod` (fn) was UNSOUND (the checker resolved `v` to the value and `v + 1` type-checked,
but the runtime bound the function and faulted `cannot apply Add to function and int`), and `import f
from lib` + `import f from lib2` silently last-won. Fix: a per-module `import_binds: HashMap<String,
Span>` records every import bind-name across ALL namespaces; a second bind of an already-imported name
errors `'<name>' is already imported` (the bind-name = alias when present, so distinct names and `import
mod as alias` still pass; a missing member stays its own error). (2) **Duplicate binder in one pattern.**
`(x, x)` / `E.V(a, a)` was neither rejected nor treated as an equality constraint — it matched ANY
values and the arm was wrongly irrefutable (`f((3,9))` returned 9, not -1). Fix: `bind_match_arm` runs a
new `first_duplicate_binder` over each (non-Or, non-Wildcard) pattern and errors `identifier '<name>' is
bound more than once in this pattern` (Rust's rule); covers tuple / enum-payload / nested patterns. `_`
repeated, a name reused across SEPARATE arms, and an or-pattern `A(x) | B(x)` all stay legal. All in
`src/checker/mod.rs`; tests in `src/checker/tests.rs` (6 reject + 6 `*_ok` regression fences). `gaps.md`
"Import name collisions" + "Duplicate binding in a single pattern" → RESOLVED. Full `cargo test` +
`cargo test conformance` green; `cargo clippy --all-targets -- -D warnings` clean.

**✅ Soundness fix — refine-on-first-use is now PERSISTENT scope-wide first-use pinning (closes the
cross/post-branch `Ty::Unknown` residual).** The earlier design (entry below) was BLOCK-LOCAL: a
refine pin inside a conditionally-run body was snapshot/restored so it did not leak past the branch,
leaving cross/post-branch heterogeneous builds uncaught. Now the FIRST mutating op that fixes an empty
collection's element/key/value type **pins it for the binding's whole scope**, even across sibling
branches/arms — building a heterogeneous collection split across branches is a hard type error, exactly
like the literal `[1, "s"]`. Checker-only fix (`src/checker/mod.rs`): removed the
`snapshot_refinable`/`restore_refinable` barrier at the THREE STATEMENT-position sites — `check_block`
(if/else/while/defer), the `for` body, and statement-`match` arms (`check_match`, Option B: a cross-arm
conflict is a hard error). The pin already targets the binding's OWNING scope (`repin`), so it survives
`pop_scope` (which only removes inner-block-declared bindings — lexical scoping intact). The two
EXPRESSION-position sites (`infer_if_else`/`infer_match`) KEEP their barrier: a value-arm produces a
VALUE, so a pin in one value-arm must not leak to a sibling value-arm (would corrupt branch value
inference). Accepts the zero-trip / always-runs over-approximation by design (`xs:=[]; for i in []:
xs.push(1); xs.push("s")` rejects even though the body never runs — sound static over-approximation).
**New narrow residual** (documented in `gaps.md`): a differently-typed push done as a SIDE EFFECT inside
sibling if-EXPRESSION / match-EXPRESSION value-arms is still not caught (rare — a value-arm is a single
expression, the mutating ops are statements). Checker-only ⇒ VM==interp parity automatic. Tests:
`flow_sensitive_{if_else_int_vs_str,map_if_elif,set_if_else}_rejects`,
`refine_inside_block_persists_then_conflict_rejected`, `refine_{single_arm_then_concrete_use,
conflict_in_second_arm,stmt_match_arm_conflict,loop_body_pin_then_post_loop_conflict,
zero_trip_loop_over_approximation}_rejects`, `expr_arm_pin_independence_ok`; must-stay-green
`refine_inside_block_on_outer_list_ok` etc. All 2444 tests + conformance + clippy clean.

**✅ Soundness + tooling — un-constrained empty collection now errors (PART A) + retroactive hover for a
refined empty (PART B)** (`auto-task/empty-coll-infer`, checker-only, VM==interp parity-neutral). Two
related improvements to empty-collection element-type inference, sharing one end-of-scope finalize seam.
**PART A:** a bare `b := []`/`{}`/`Set()` whose element/key/value slot is NEVER inferred (only read into
an untyped sink — `print(b)`, `b.len()`) used to type-check silently as `List[Unknown]`; it is
now a static error `cannot infer element type of empty collection; add a type annotation`. Mechanism: the
let-handler's un-annotated branch records a pending site `(owning_scope_idx, name, decl_span)` in
`empty_coll_sites` when the declared type is an empty literal shape (`is_unrefined_empty_coll` — a
List/Set/Map whose DIRECT slot is bare `Unknown`; `[[]]`=`List[List[Unknown]]` is NOT empty and excluded,
as are `None`/nullary-variant `Unknown`-in-slot producers), gated `!inferring_ret` so return-inference
passes don't double-record. A later **constraining** op clears it via `drop_empty_site(name)`: the two
refine gates (`refine_receiver`/`refine_index_receiver`, before their speculative-error truncate-returns,
so an erroring mutator arg like `xs.push(undefined)` still drops the site and its exactly-one-error tests
stay green), AND — so the rule never rejects a binding that *is* constrained, just not through a mutator —
a concrete-typed value flowing into the binding: a whole-binding reassignment / compound-assign /
tuple-assign (`check_assign`'s Ident arm, gated on the value being fully concrete so reassigning *another*
empty `b = []` does NOT clear it), or passing/returning it into a CONCRETE collection sink (a typed param
in `check_args_range_w`, a typed `return` in `check_return`). `finalize_empty_coll_sites` runs before
`pop_scope` at the fn-body + module seams and errors on any still-unrefined site owned by the popping
scope. **False-positive guards fall out structurally for the literal sinks** (annotation
`b: List[int] = []`, typed param `f([])`, typed `return []`, turbofish `List[int]()` leave no
`Unknown`-in-slot or bind no local → never recorded) **and are dropped explicitly for the one-binding-away
sinks** (`b := []` then `f(b)` / `return b` / `b = [1,2]` / `a, b = [1], [2]`). A post-merge adversarial
review found the one-binding-away drop missed the case where the empty binding is read as an **RHS value
that escapes** into another binding/structure — `c = b` / `bx.items = b` (assign), `c := b` (alias),
`c := [b]` (nested in a literal) — spuriously erroring on `b` though the program is type-sound; fixed with
`drop_value_escape_sites(value)` at the let + assign seams (drops the source ident's site; the alias
records its own if it stays unrefined, so the requirement *moves* rather than vanishes — no false-negative).
A terminal non-escaping read (`print(b)`, `b.len()`) is intentionally NOT a drop, so the headline error
still fires. Scope coverage is fn-body
+ module (an empty declared inside an if/for/match body that pops before the seam is a documented
residual, matching the refine machinery's block-local limits). **PART B:** retroactive hover — when the
probe lands on an occurrence of a binding whose recorded type still carries `Unknown`-in-slot,
`hover_record_binding` does NOT lock `hover_result`; it stashes `(owning_scope_idx, name, kind, doc)` in
`hover_pending`, and `finalize_hover_pending` (same seam) overwrites `hover_result` with the binding's
FINAL refined type via `lookup`. So hovering the `b := []` decl (or any use before `b.push(0)`) now shows
`List[int]`, not `List[Unknown]`. The owning-scope index gates the finalize (`owning >= idx`, mirroring
`finalize_empty_coll_sites`): a post-merge review caught that without it, an intervening fn/method
`check_fn_body` seam between a module-level empty decl and its refining op would prematurely lock the
hover to the still-unrefined type — so the finalize only resolves at the seam that OWNS the pending
binding (regression test `hover_refined_empty_decl_intervening_fn_shows_final_type`). Entirely
`hover_probe`-gated → parity-neutral by construction. Tests: `checker::tests`
`unconstrained_empty_{list,map,set,at_module_level}_rejected` + full typed-sink ok matrix
(`typed_annotation_*`, `typed_param_empty_arg_ok`, `typed_return_empty_ok`, `turbofish_empty_ctor*`,
`empty_push_then_read_no_false_error`) + the one-binding-away constrained matrix
(`empty_then_{plain_reassign,compound_assign,tuple_assign,reassign_from_call,conditional_reassign}_concrete_ok`,
`empty_binding_into_typed_{param,return}_ok`, and the `empty_then_reassign_still_empty_rejected` guard);
`editor::tests::hover_refined_empty_{decl,pre_use}_shows_final_type`.
Annotated the 3 shipped examples that relied on the old permissiveness and have no later constraint
(`edge_cases.chz`, `map.chz`, `concurrent_collection_test.chz`); `bst.chz`'s `walk := []` stays
un-annotated (its `inorder(root, walk)` call into a `List[int]` param now constrains it). All tests +
conformance + clippy clean.

**✅ Soundness fix — empty-collection / nullary-variant / `None` `Ty::Unknown` slot is now closed via
FULL refine-on-first-use + insertion-site Hashable check + (originally BLOCK-LOCAL, now PERSISTENT —
see the entry above) flow-sensitivity (the
empty-slot half of the `Ty::Unknown`-is-assignable family; sibling to the recursive-return fix below).**
A bare empty literal (`[]`/`{}`/`Set()`), a nullary user-enum variant (`Box.Empty`), or native `None`
typed its element/key/value/type-arg slot as the permissive `Ty::Unknown`, which nothing later refined —
so `x:=[]; x.push(1); x.push("s")` passed `check` then faulted at runtime, and the deliberate
float-key/Hashable ban was bypassed (`m:={}; m[1.5]=...`, `s:=Set(); s.add(nan)`). Fix (checker-only,
`checker/mod.rs`): `refine_receiver` (top of `infer_method_call`) and `refine_index_receiver`
(`check_assign` Index branch) — when a **simple-variable** binding's type carries `Unknown` in a slot
(detected by `contains_unknown_in_slot`, recursing through list/set/map/Option/Result/tuple/Channel/
Shared/Atomic and user generic struct/enum), the FIRST mutating op (`.push`/`.add`/`.insert`/`.extend` /
`x[k]=v`) that supplies a concrete type RE-PINS the binding at that slot via `merge_unknown` (which
recurses into nested type params — `List[Option[Unknown]]` + `Some(5)` → `List[Option[int]]`, `[Box.Empty]`
+ `Box.Full("hi")` → `List[Box[str]]`). A later INCOMPATIBLE concrete type is then a normal `check_args`
mismatch, enriched to hint at annotating for a mixed/protocol collection. Heterogeneous/protocol
collections now REQUIRE an explicit annotation (`shapes: List[Shape] = []`) — intended and clearer.
Non-Hashable keys/elements are rejected by a DIRECT insertion-site `is_hashable_key` check at `m[k]=v`
(fires even while the key type is still `Unknown`) and at set-element concrete-ification. **Flow-
sensitivity** (now PERSISTENT scope-wide first-use pinning — see the entry above; originally block-local
via `snapshot_refinable`/`restore_refinable`): a refine pin at a STATEMENT-position site (`check_block`,
the `for` body, statement-`match` arms) now PERSISTS for the binding's whole scope, so `xs:=[]` + `if c:
xs.push(1) else: xs.push("s")` is **rejected**; the EXPRESSION-position arms (`infer_if_else`/
`infer_match`) keep their restore so value-arms refine independently.
**Residuals** (documented): simple-variable-receiver-only (`obj.field`/`f()`/`xss[0]` unrefined), and
side-effect pushes inside sibling EXPRESSION-position arms (the cross/post-branch STATEMENT leak is now
closed). **Golden-test
checker-bypass fixed:** the golden tests drive `run_capture`, which BYPASSES the Checker, so a checker
regression on a shipped example shipped falsely green — added `checker::tests::all_shipped_examples_typecheck`
(build_graph + check_graph over every `examples/*.chz`, two intentional run-only demos `panic.chz` /
`explicit_type_args.chz` allow-listed) and annotated `examples/poly_method.chz` `List[Shape]` under the
new rule. Checker-only ⇒ VM==interp parity automatic (newly-failing programs fail `check` before either
engine runs; passing programs run byte-identical). All 2394 tests green; clippy + conformance clean.
`gaps.md` updated (empty-collection + generic-nullary-variant producers RESOLVED; all three `Unknown`-in-slot
producers now closed).

**✅ Soundness fix — return-type inference is now ORDER-INDEPENDENT (fixpoint), closing the
recursive/forward-reference half of the `Ty::Unknown`-is-assignable hole.** The checker inferred
function/method return types in a single SOURCE-ORDER pass and bailed to `Ty::Unknown` whenever the
deciding `return` was a call to a not-yet-inferred function (a forward reference, or mutual recursion).
`Unknown` is universally assignable, so a bogus return flowed check-blessed into a typed slot and
faulted at runtime (`fn rec(n:int): if n<=0 return base(0) else return rec(n-1)` + later
`fn base(n:int): return "hello"`, then `v: int = rec(2)` wrongly passed `check` — `rec` really returns
`str`). Fix: `infer_returns` (`checker/mod.rs`) now wraps the per-pass walk (`infer_returns_pass`) in a
bounded FIXPOINT — re-infer every un-annotated fn/method until no stored `FnSig.ret` changes (cap =
un-annotated-count + 1; monotone, a concrete ret is never reverted to `Unknown`, so it converges and the
final ret is order-independent). A self-recursive call still contributes no type; the non-recursive
returns decide (so `fact`/`fib` are unchanged — base-case concrete wins). Divergent CONCRETE returns
stay the user's job to annotate (`-> T` or a protocol existential `-> Stringable`); with no annotation
conflicting concretes are an `expected return type …, found …` error — **no union types**. A genuinely
un-inferable un-annotated fn/method (pure self-recursion, or mutual recursion with no concrete base
anywhere — ret stays `Unknown`) keeps a **permissive** type, NOT rejected: a blanket "leftover Unknown
⇒ require annotation" check over-reaches (bare `Unknown` is also produced by non-recursive paths like
`return x[0]` of an empty collection, and by already-errored bodies), so soundly rejecting only the
recursive-no-base case needs call-graph cycle detection — tracked as a follow-up. Checker-only change ⇒
VM==interp parity automatic. `gaps.md` "Ty::Unknown is treated as assignable" updated (recursive-return
producer RESOLVED; empty-collection = sibling task, generic-nullary-variant remains). Tests green;
clippy + conformance clean.

**✅ Soundness fix — string-interpolation fragments are now type-checked (was a CRITICAL compiler
panic + unsound `check`).** The checker treated an interpolated `str` as opaque `Ty::Str` and never
resolved/type-checked the `{…}` fragment exprs, while the compiler hard-assumed the checker already
rejected undefined names — so `print("{nope}")` passed `check` then panicked the compiler at
`global_slot` (`compiler/mod.rs`), and every type/method/arity error inside `{…}` escaped `check`
entirely. Fix: the `ExprKind::Str` arm now parses the literal with the shared interpolation parser and
`infer_value`s each fragment (`checker/mod.rs::check_interpolation`), so undefined names + type errors
surface as compile errors at the string's span and `global_slot`'s invariant holds (panic impossible).
The compiler's private interpolation parser (`Chunk`/`parse_interpolation`/`parse_expr_str`) was
extracted into a new shared leaf module `src/interpolation.rs` (neutral `InterpError`; compiler and
checker each map it to their own error type) so both engines chunk strings byte-identically — two-engine
parity preserved (no `interp` edit needed; the new check is a pre-run gate). Pinned by
`checker::tests::interpolation_{undefined_name_rejected,type_error_rejected,valid_ok}`. Full `cargo
test` (2365) + `cargo test conformance` green, `cargo clippy --all-targets -- -D warnings` clean.

**✅ `chezzi docs` + `module:function` entrypoint + stdlib reference (tooling/docs).** Three related
changes: (1) **`chezzi docs [topic]`** prints embedded language docs — topics `spec`/`syntax`/`stdlib`,
and a bare `chezzi docs` (or `docs llms`) emits the full reference bundle (spec+syntax+stdlib) for
piping to an LLM. Docs are `include_str!`-embedded so the
binary is self-contained; logic is a pure `render_docs` (unit-tested), `cmd_docs` just prints/maps to
`ExitCode`. (2) **`module:function` entrypoint:** `chezzi.toml`'s `entrypoint` now accepts a
`:function` suffix (`"src.main:main"`) — a bare `chezzi run` runs the module top-level and then calls
that function (missing/non-function = clear error), so the source needs no trailing call and you can
swap which function runs via the manifest. Bare `"src.main"` keeps the old run-top-level behavior;
explicit `chezzi run <file>` is always top-level-only. Implemented via `main::split_entrypoint` +
`vm::invoke_entrypoint` (reuses `invoke_value`/`entry_home`) threaded through a new
`run_file_with_entry`; the old `run_file_with`/`run_file_parallel` became `#[cfg(test)]` parity-test
helpers. Scaffold now writes `entrypoint = "src.main:main"` and a `main.chz` with no trailing call.
(3) **New [`docs/stdlib.md`](docs/stdlib.md)** — the previously-undocumented stdlib/builtin surface
(global builtins, per-type methods, runtime types, native + pure-Chezzi `std.*` modules); `syntax.md
§13` shrank to a pointer + orientation. Docs synced (`spec.md`, `syntax.md §9b`, `CLAUDE.md`,
`manifest.rs`). VM↔interp parity untouched (entrypoint is VM-only; no `examples/*.chz` changed).

**✅ Enum methods (mirrors the struct-method machinery end-to-end).** Enums now accept `fn name(self, …)`
method blocks after their variants, parsed via the same `parse_fn(true)` path structs use; the parser
enforces variants-before-methods. (`test fn` is **rejected** in enum bodies — enum test *suites* are not
wired in the compiler/test-runner, so a `test fn` would silently never run; rejected at parse time as a
follow-up. A `Hashable` enum's `hash(self)` is dispatched at runtime in both engines, so `Set[E]`/`Map[E,V]`
keys work — not just type-check.) The checker gained a name-keyed
`enum_methods` map (+ `EnumSigInfo.methods` ferried across the module boundary on both the whole-module
and `from`-import paths) and a `Ty::Enum` arm in `infer_method_call` (with generic-enum `T`-substitution),
in `satisfies_args` (cloned from the struct arm into a shared `satisfies_methods` helper — unlocks
`Stringable`/`Hashable`/`Add`/`Sub`/`Mul`/`Comparable` for enums and protocol-bound generics), and in
`op_overload_result`/`ordering_allowed`. The desugar collectors (`collect_methods*`, `validate_defaults`,
the walk) now treat struct + enum methods uniformly (name-keyed; `normalize_call` unchanged). Both engines
bind the whole enum value as `self`: the VM added `Program::enum_methods`/`enum_home`, an `Obj::Enum` arm
in `do_method_call`, a shared `resolve_overload_method` used by `struct_arith`/`struct_compare`, and the
`str(self)` Stringable hook in `stringify`; the interp mirrors all of it (`enum_defs` registry, an enum
branch in `call_struct_method`, its own `resolve_overload_method`, the stringify hook) — kept byte-identical
(golden `examples/enum_methods.chz` runs on VM + interp + parallel + `.expected`). **Follow-up lever:** the
method IC is skipped for enums (type-erased → no `tid`); enum-method dispatch uses the slow `run_proto`/
flatten path. **Out of scope (deferred):** `derive` and the multi-bound same-name-method
ambiguity diagnostic (a pre-existing struct-era wart, first-bound-wins). (Nominal `newtype` — once
listed here as deferred — **shipped in M21**; see its section below.)

**✅ Module-scoped user types (struct / enum / `type` alias).** Types are now **private to their
declaring module**, mirroring how top-level functions are namespaced — exported by default (no `pub`),
visible elsewhere ONLY via import. `import core.geo` → `geo.Point(1,2)` / `x: geo.Point` /
`List[geo.Point]` / `geo.Color.Red`; `import Point from core.geo` → bare `Point(1,2)` (rename allowed
for user types). A bare use of a type whose module was imported whole but not named-imported is a
**check-time error** with an import hint. Two modules MAY declare the same type name (no collision).
Enforcement lives in the **checker** (per-module type tables: `structs`/`enums`/`variants`/`aliases`
cleared per module + re-injected via `bind_import`; `ModuleSig` carries resolved struct/enum/alias
defs; reverse `types_by_name` index drives the hint; new `Type::Qualified{module,name,args}` AST +
parser `m.T[args]` production). Runtime keying is the **always-qualified identity key + bare display name** model (ROOT REDESIGN,
2026-06 — replaced the old "Option C" bare-key/disambiguate-on-collision scheme, which was a bug
factory: the key doubled as the printed name, so consumers had to know bare-vs-qualified and several
got it wrong, e.g. `json.decode` decoding a collision-loser against the WRONG layout). The new design
**separates identity from display**: (1) **IDENTITY KEY** is ALWAYS `<module-key>::Name` for EVERY user
struct/enum/variant/alias — no winner/loser, no bare keys, unique by construction (the module key is
the declaring module's dotted path or the entry file's stem, from the shared
`resolver::module_keys(graph)`, deterministic + `#idx`-tiebroken so all three engines derive it
byte-identically). The compiler, checker, both engines, AND the `--parallel` snapshot/wire format key
every layout table (`Program::structs`/`variants`, checker tables, interp `struct_fields`, …) by this
ONE key; the value's runtime tag carries it. (2) **DISPLAY NAME** is the bare `Name`, stored on the
def (`StructDef::display_name`): ALL user-facing output — print/`str`/stringify, errors, `json` ENCODE,
`repr` — renders it, so output is **byte-identical** regardless of module and two colliding `Point`s
both print `Point(...)`. Because there is ONE canonical key, the whole bug class vanishes structurally:
`json.decode` (`json_decode::DecodeEnv`, implemented by both engines) resolves the target — and nested
struct-field types **in their own DEFINING module's scope** — to the qualified key, tags the produced
struct with it, and decode errors render the bare name. RESERVED/NATIVE types
(`Result`/`Option`/`Some`/`Ok`, `Ref`, `Iterator`, `Match`/`Response`, the std type surface on
`import std.*`, and the FFI width names) are **not** module-keyed — they keep their bare name (the
qualification pre-pass skips std/native modules). A match pattern `Color.Red` against a whole-module-
imported enum is resolved **SCRUTINEE-DRIVEN** on every engine: the matched value carries its own
qualified enum identity key (the very enum the checker resolved the scrutinee to), and an arm matches
iff its written qualifier equals that key's BARE form (interp `try_bind`: `bare_display(ty)==en`; VM
`match_arm`: the M19 int-id fast path, with a `bare_display(enum_key)==enum_name` fallback baked into
`Op::MatchArm.enum_name` on an id MISS). It is NEVER re-guessed by iterating the (RandomState-seeded)
import map — doing so ignored the scrutinee and picked nondeterministically (often the WRONG enum when
two whole-imported modules declared a same-named enum); the construction side (`enum_bare_key`) still
resolves against the current module context, which is correct. The same deterministic key map + per-module bare-visible-type set
is computed identically by all three engines, so the cooperative VM, `--parallel`, and the interp agree
on every key (3-engine parity, incl. a genuine collision: field access, method call, `match`, AND
`json.decode` on a colliding type, plus a cross-airlock imported-type value). The runtime `bind_import`
(both engines) binds a
member's value when the TARGET module exports one and skips only a value-less TYPE member (so a
`from`-imported fn named like another module's type still binds); the bare constructor fires only for
a type bare-VISIBLE in the importing module. Imported `type` aliases are **transparent** (body
resolved in the defining module's scope, carrying the FFI-width license; an unlicensed alias embedding
an un-imported width is rejected at import). Reserved/native types (`Result`/`Option`/`Some`/`Ok`,
`Ref`, the std type surface on `import std.*`, FFI widths) stay global/bare always. New grammar
production in `docs/grammar.bnf` (`conformance` green). Docs: `docs/spec.md` + `docs/syntax.md`
(Imports). This is a **pre-JIT sequencing gate**, not a feature freeze — new language work can still land.

**✅ Redesign follow-up — two regressions fixed (2026-06).** The qualified-identity-key redesign
introduced two bugs (caught by adversarial review, reproduced on the built binary), now fixed: (1)
**checker errors leaked the qualified IDENTITY key** (`type single::Point has no field 'nope'`) — the
identity-vs-display split was applied at runtime stringify but NOT in the checker's `format!("type
{ty} …")` paths; fixed at the single choke point — `Ty`'s `Display` for `Struct`/`Enum` now renders
`bare_display(n)`, so every field/method/type-mismatch error (single- and cross-module) prints the
BARE name. (2) **bare match-pattern enum was resolved NONDETERMINISTICALLY** by iterating the
RandomState-seeded import map (scrutinee-blind), alternating wrong-arm / `MatchNoArm` crash across
identical runs and disagreeing between engines — now **scrutinee-driven** (see the match-pattern
resolution note above), deterministic + identical on VM / `--serial` / `--parallel` / interp.

**✅ CLI cleanup + parsed `chezzi.toml` entrypoint (5 scoped changes; no engine/semantic change).**
Quality-of-life + a small manifest reader, zero new deps. (1) **Sample-string rename** `"thuan"` →
`"chezzi"` across docs/examples/tests (input + expected kept in sync; width-10 format examples in
`docs/syntax.md` recomputed for the 6-char name). (2) **Milestone tags removed** from the `chezzi help`
COMMANDS block. (3) **`--interp` CLI flag dropped** — the tree-walk interpreter stays as the FROZEN
two-engine parity oracle (golden VM-vs-interp tests call it directly), but it has no CLI surface; `mod
interp` is now `#[cfg(test)]` (test-only, where every reference lives). (4) **Hand-rolled
`chezzi.toml` parser** (`src/manifest.rs`): a tiny fixed-schema reader — `[section]` headers,
`key = "value"` string pairs, `#` comments; captures `[project]` `name`/`version`/`entrypoint`; an
EMPTY manifest parses to all-`None` (the existing root-marker fixtures stay valid); malformed lines
are a clean `Err`. (5) **Bare `chezzi run` runs the manifest entrypoint**: with no file argument it
walks up from the cwd for `chezzi.toml` (`resolver::find_root_from_dir`), parses it, requires
`[project] entrypoint` (a dotted module path), and resolves it root-relatively via
`resolver::module_file` → e.g. `<root>/src/main.chz`, then runs it on the VM honoring all flags.
Imports stay **root-relative** (`build_graph` walks up to the same marker) — locked by a tempdir test
(`entrypoint_imports_are_root_relative`: `import lib` → `<root>/lib.chz`, `import src.utils.common` →
`<root>/src/utils/common.chz`). `chezzi init` now scaffolds an **active** `entrypoint = "src.main"`,
so a freshly-init'd project runs with a bare `chezzi run`. Verified end-to-end: `init` a tmp project →
bare `chezzi run` (+ `--serial`, + nested-cwd) prints `Hello from Chezzi!`, `chezzi run src/main.chz`
unchanged, `chezzi test .` passes, `chezzi run --interp` → `unknown flag`, `chezzi help` shows no
`(M..)` tags/`--interp`. Docs: `docs/spec.md`, `docs/syntax.md`, `CLAUDE.md`, this file.

**✅ Project tooling — `install.sh` + `chezzi init [dir]`.** Quality-of-life, no runtime/semantic
change, no new deps. `install.sh` (POSIX `sh`, `set -e`, executable) guards for `cargo` on PATH
(hinting https://rustup.rs if missing), then `cargo install --path .` and reminds the user to keep
`~/.cargo/bin` on PATH. `chezzi init [dir]` (new `cmd_init` + pure `scaffold_project` in `src/main.rs`,
unit-tested against a TmpDir) scaffolds `chezzi.toml` + `src/main.chz` (`fn main():` + a top-level
`main()` call — no auto entrypoint) + `src/main_test.chz` (`test fn` + `assert`); `dir` defaults to `.`,
is created if missing, and an existing `chezzi.toml` is refused (no clobber). The manifest is both a
root marker AND a parsed manifest (see the CLI-cleanup entry above): the toolchain reads its
`[project]` keys, and `entrypoint` (scaffolded active as `"src.main"`) drives a bare `chezzi run`;
`run <file>` stays top-to-bottom and `test` still discovers `*_test.chz`. Verified end-to-end:
`chezzi init <tmp>` → `chezzi run <tmp>/src/main.chz`
prints `Hello from Chezzi!` → `chezzi test <tmp>` reports `2 passed`, and re-`init` refuses with a
non-zero exit. Docs: `docs/syntax.md` §9b, `docs/spec.md` (module-resolution section), `CLAUDE.md`.

**✅ Generator return-type inference (`-> Iterator[T]` now OPTIONAL) — Q1 (2026-07-07).** A generator
(a `fn` whose body uses `yield`, auto-detected via `is_generator`) no longer MUST declare `-> Iterator[T]`:
with no return annotation the element type `T` is **inferred from the FIRST `yield`** (strict-first-yield,
mirroring late list `[]` element inference), and every later `yield` is validated against it. Wired
through the EXISTING return-inference machinery so callers SEE the type: `infer_fn_ret`
(`src/checker/sig.rs`) now, for a generator, routes to a new `infer_generator_ret` that returns
`Iterator[first_yield]` and writes it back into the stored `FnSig.ret` (via `infer_returns_pass`, the
fixpoint that also handles forward/mutual refs and the struct/enum-method arms) — so `for x in count()`
binds `x: int`. A new `in_generator: bool` Checker flag (distinct from `yield_ty`, which is `None` during
inference) is the sole in-bounds signal for `yield`: `check_yield` COLLECTS yields into `collected_yields`
while inferring, and validates against the pinned `T` in pass 2 with **plain `assignable`** (no
`CoerceFloat` at a `yield`). SOUNDNESS: (1) `yield 1` then `yield 2.0` is REJECTED (`expected yield type
int, found float`) — no silent int→float join that would run int-under-float; (2) an un-inferable element
(`yield []` alone, or a residual `Unknown`) errors `cannot infer generator element type; annotate the
return type as Iterator[T]` via `fill_ret`'s `bad` flag — never a silent `Iterator[Unknown]` leak. The
explicit `-> Iterator[T]` path is untouched (skipped by `infer_returns`, validated in `check_fn_body`), the
bare-`return`-only restriction stands, and closure boundaries reset `in_generator=false`. **Zero VM/compiler
change** (the compiler reads only `decl.is_generator`; bytecode is byte-identical to an annotated generator),
so both engines are parity-clean by construction. Tests: checker `generator_infers_element_type_no_annotation`
/ `generator_inferred_element_recovered_not_unknown` / `generator_inferred_int_then_float_rejected` /
`generator_uninferable_element_rejected` / `generator_inferred_struct_method_no_annotation` /
`generator_explicit_annotation_still_works`; VM (serial + M:N) `vm_generator_inferred_no_annotation` /
`vm_generator_inferred_struct_method` / `golden_generators_inferred_chz`; golden
`examples/generators_inferred.chz` (auto-covered by `all_shipped_examples_typecheck`). Docs: `docs/syntax.md`
(yield block), `docs/spec.md` (generators section). Grammar unchanged (no new syntax; `cargo test
conformance` green).

**✅ Formal `Iterable[T]` protocol + `.iter()` cursor (owner-requested; the decoupled follow-on the
constructors work flagged).** Additive — nothing existing changes behavior; 3-engine parity throughout.
The win: a plain collection now composes into the SAME lazy adapter pipeline as a hand-written struct
iterator (`Take([10,20,30,40].iter(), 2)`, `Mapped([1,2,3].iter(), fn)`) — impossible before, since
you can't call `.next()` on a `list`. Wired (mirroring the `bytes`/`bytearray` Obj/Value pattern):

- **`Iterable[T]` prebuilt protocol** `{ iter() -> Iterator[T] }` — reserved + registered next to
  `Iterator[T]` (unchanged). The looser sibling: `Iterable` promises only a cursor; `Iterator` also has
  `next`, so every `Iterator` IS `Iterable` (`iter()` returns self). Conformance via `iterable_elem`
  (collections + any `Iterator` intrinsically via `iter_elem`, + a struct with structural `iter`).
- **Cursor heap object** — VM `Obj::Iter { items: Vec<Value>, pos }` (32B, 88B-guard green) and interp
  `Value::Iter(Rc<RefCell<IterCursor>>)`. The TYPE is the existing `Iterator[T]` existential — NO new
  `Ty`. GC-**NON-LEAF**: `children()` traces `items` (contrast `Bytes`/`ByteArray` leaves) so a
  not-yet-consumed snapshot element survives a collection. `.next()` → `Some(items[pos])` + advance,
  idempotent `None` past the end. deep_clone → a fresh in-task copy (airlock).
- **`.iter()` dispatch** — on `list`/`set`/`map`(→keys)/`str`(→char)/`bytes`/`bytearray`(→int): a FRESH
  cursor SNAPSHOTTING current contents in EXACTLY `for x in X` order (reuses `drain_iterable` /
  `iter_rows_from_value`, the for-loop's single source of truth). On any `Iterator[T]` value (cursor,
  generator, `next`-struct): returns SELF (idempotent). `List(xs.iter())`/`Set(...)` drain for free.
- **For-loop additive case** — a struct with `iter()` but NO `next()` is for-iterable via a one-time
  `.iter()` then the cursor drains: checker for-bind arm AFTER the `next` arm (a struct with BOTH keeps
  the `next()` fast path — back-compat precedence); VM `Op::IterableToCursor` (one-time, before the
  per-iteration loop — structs-with-`next`/generators pass through byte-identical); interp `exec_for` /
  `drain_value_to_rows` sibling branch. The hot collection / `next`-struct paths are untouched.
- **Sendability** — a cursor IS sendable: it crosses the `spawn`/channel airlock as a DEEP COPY, like a
  `list`. `to_wire`/`from_wire` carry a `WireValue::Iter { items, pos }` (items recursively wired, `pos`
  carried) and `to_snap`/`replay_snap` a `SnapValue::Iter`; the interp's `deep_clone` already deep-copies
  the cursor identically, so all three engines agree. A cursor over a non-sendable element (e.g. a
  generator) faults recoverably via the recursion, exactly as a `list` of that element would. (`sendable_rec`
  is UNCHANGED — a cursor reuses `Iterator[T]`'s type, already sendable; no static change was needed. An
  earlier cut gated the cursor non-sendable like a generator, which panicked the spawned VM worker while
  the interp succeeded — a parity divergence, now fixed.)
- **Generator airlock = graceful runtime error, never a panic** — a frame-holding generator (a value from
  calling a generator `fn`) shares the `Iterator[T]` existential with a cursor, so the checker cannot
  distinguish them; the RUNTIME is the enforcement point. A generator crossing **any** airlock-out site
  raises a catchable `a generator cannot be sent across tasks` error with the real spawn/nursery-site span:
  `to_snap`/`snapshot_modules`/`ensure_snapshot` are now fallible (the choke point re-stamps `to_wire`'s
  placeholder `Span{0,0}` with the nursery span; `ensure_snapshot` memoizes only on success), and the
  smuggle sites (`deep_clone` for `spawn` args/`spawn:` captures, `Op::NewShared`, `new_atomic`,
  `Channel.send`/`try_send`, `Shared.set`/`update`, `Atomic.store`/`exchange`/`cas`, plus `wire_args` /
  `wire_callable` for spawn-method args + `Executor.submit` closure captures) re-stamp via a shared
  `to_wire_at` helper. These are all DIRECT crossings (a generator moving across the airlock as data);
  they fault on **both** engines and are unchanged.
- **Module-global generator = Option B "gated iff reachable" (2026-07-08).** A generator held as a
  module GLOBAL is a special case: the M:N engine eagerly snapshots EVERY module global at the first
  nursery, so a module-level generator + any `parallel:` previously faulted even when **no task touched
  it** — a serial-vs-M:N divergence (serial never snapshots, ran clean). Fixed with two coordinated
  `src/vm/sched.rs` changes: (1) `to_snap`'s Generator arm now yields `SnapValue::Poison` (replays as
  `nil`) instead of erroring — an M:N worker can never obtain a real cross-heap generator from a module
  global (memory safety, gate-independent); (2) a conservative reach gate
  `Vm::check_task_generator_reach` at `register_task` (the single common spawn choke, run on **both**
  engines during body execution) faults with the graceful `a generator cannot be sent across tasks`
  error IFF a spawned task can reach a generator-embedding global — a direct
  `GetGlobalSlot`/`SetGlobalSlot` of that slot, or **any** op that transfers control into an unscanned
  proto (a call / operator overload / protocol-`str` hook / nested spawn-defer / `GetCaptured`
  home-global read) treated as OPAQUE. `has_generators`-gated (zero cost when the program has no
  generator) + an `any_module_global_embeds_generator` presence short-circuit. An **untouched**
  generator global now runs clean on both engines; a **reached** one faults identically on both — parity
  by construction. Conservative (over-gates, never under-gates); a callee-provenance-paired transitive
  scan is a documented future precision refinement. Two soundness fixes over the first cut
  (adversarial-review, 2026-07-08): **(i) `print` is not blanket-inert** — `CallPrint`/`CallPrintSep`
  stringify their operands and a struct/enum/newtype `str(self)` hook runs arbitrary code (a hidden
  call) that can read a generator global, so a print is a reach whenever some `str` hook could reach one
  (`any_str_hook_reaches_generator`), while a print with no generator-reaching hook stays inert (keeps
  the `print("literal")` SAFE case un-gated); the first cut allowlisted print and UNDER-gated a
  str-hook-printing task (serial ran the hook, M:N read Poison→Nil → diverged). **(ii) join-time
  re-gate** — the gate also runs at the LAZY nursery join (`join_nursery`, before the serial-coop vs M:N
  split), closing a TOCTOU: a lazy nursery's tasks run — and the M:N snapshot is taken — at the join, so
  a module global **reassigned to a generator between `spawn` and the join** slipped past a spawn-time-
  only gate (serial ran the real generator, M:N read Poison→Nil → diverged). **(iii) nested-nursery
  conservative re-gate** (adversarial-review charges #1/#2/#3, 2026-07-08): the join-time re-check in
  fix (ii) only covered the LAZY fall-through of `join_nursery` (this nursery's OWN tasks), not the
  early-enlisted OUTER nursery path (`early_enlist_outer` → `join_enlisted_scope`). When an INNER
  nursery joins while an OUTER nursery is pending, M:N EARLY-ENLISTS the outer task against the frozen
  module snapshot (taken at the inner join) while serial runs it at its own later join against LIVE
  globals — so a generator reassigned across the nested nurseries diverged (serial faults, M:N reads a
  stale frozen value: `nil` if it was a generator at snapshot time, or the wrong non-generator value if
  reassigned after). Closed by `Vm::check_outer_pending_generator_reach` (called at the top of
  `join_nursery`, on **both** engines): a still-pending outer task reaching **any** module global — or
  an OPAQUE callee, or any `print` — faults NOW. The verdict is purely STATIC (proto code +
  `has_generators`), so it is identical at every join on both engines → parity by construction
  (over-gates an outer task reaching a non-generator global when any generator body exists; never
  under-gates). Tests (serial + M:N, all assert parity): `generator_module_global_with_nursery_is_graceful_vm`
  (SAFE, both clean), `generator_module_global_hazard_reads_it_faults_both` (direct read + `recover:`),
  `generator_module_global_transitive_helper_reads_it_faults_both` (transitive/OPAQUE),
  `generator_module_global_over_gate_guards` (non-gen global read + generator LOCAL both stay clean),
  `generator_module_global_str_hook_print_faults_both` (fix i: hook reach via `print` + innocent-hook
  clean), `generator_module_global_reassigned_after_spawn_faults_both` (fix ii: single-nursery TOCTOU),
  `generator_module_global_nested_nursery_reassign_before_faults_both` +
  `generator_module_global_nested_nursery_reassign_after_faults_both` (fix iii: cross-nursery TOCTOU,
  reassign before/after the inner block), `nested_nursery_outer_reads_global_no_generator_ok_both` (fix
  iii non-regression: generator-free nested nursery stays clean); the 6 direct-crossing goldens
  strengthened to assert both engines fault. **(iv) EXECUTOR task-entry gate** (adversarial-review,
  2026-07-08): the reach gate was wired into the NURSERY choke points only — an `Executor` job that read
  a generator module-global slipped through un-gated (serial ran the real generator, M:N replayed the
  poisoned global as `nil` → the misleading `cannot iterate over nil` + a serial-vs-M:N divergence).
  Closed by gating `Executor.submit` at the submit site (`executor_method`, the same
  `check_task_generator_reach` the nursery's `register_task` uses — an `Executor` job is a zero-arg
  closure, wrapped in a no-arg `PendingCall::Call`) AND re-gating the whole queue at `shutdown`
  (`gate_executor_queue`, the `Executor` analogue of the lazy-nursery join re-gate: reads globals as
  they stand at drain, before the M:N `drain_executor_on_pool` snapshot freezes them, closing the
  submit→shutdown TOCTOU). Both run on the host VM on both engines → parity by construction; `has_generators`
  + `any_module_global_embeds_generator`-short-circuited (zero cost / no per-job `from_wire` for
  generator-free programs). Tests: `generator_module_global_executor_reads_it_faults_both` (direct read
  + `recover:`), `generator_module_global_executor_untouched_runs_clean_both` (innocent job stays clean),
  `generator_module_global_executor_transitive_helper_faults_both` (transitive/OPAQUE),
  `generator_module_global_executor_reassigned_before_shutdown_faults_both` (submit→shutdown TOCTOU).
  Docs: `docs/spec.md`, `docs/concurrency.md`, `docs/concurrency-b3.md` §5.1.
- **NON-GOALS (documented, not built):** multi-pass/single-pass TYPE SAFETY (unfixable without
  move/ownership — `count_twice([list]) == 6` via two independent cursors vs `count_twice(generator) ==
  3` consumed once; each `.iter()` is fresh, but reusing an exhausted cursor yields nothing); auto-
  `.iter()` inside adapters (v1 requires explicit `xs.iter()`); routing builtin for-loops through
  `.iter()` (the fast path stays); cursor `.reSet()`/`.peek()`/`.rev()`/`size_hint`.
- **grammar.bnf intentionally UNCHANGED** — `.iter()` is the existing method-call production, no new
  syntax (`cargo test conformance` green).
- **Tests/golden:** checker `iter_method_on_collections_types_as_iterator` /
  `iterable_bound_accepts_list_and_generator` / `iter_idempotent_on_generator_and_cursor` /
  `iterable_struct_with_only_iter` / `iter_cursor_drives_existing_adapters`; VM/interp parity
  `iter_next_idempotent_both_engines` / `iter_snapshot_order_matches_for` / `cursor_composes_into_adapter`
  / `for_over_pure_iterable_struct` / `list_of_cursor_roundtrip_both_engines` /
  `cursor_crosses_spawn_airlock_three_engine_parity` / `cursor_crosses_airlock_by_deep_copy` / `generator_iter_returns_self_vm`;
  GC `obj_iter_traces_items_as_gc_children`; `examples/iterable.chz` + `.expected` goldened 3-engine.

**✅ Checker — declared-non-void fn must return a value on every path (Option B).** A function body is a
sequence of **statements**, not an expression, so an inline body `fn a() -> int: 10` parses `10` as a
discarded expr-statement and silently falls off the end to `nil` (this was mis-filed in `gaps.md` as a
"bare fn name not callable / dispatch bug"; the real root cause is a **missing-return check** — dispatch
was always correct). The checker now rejects a function with a **declared non-void return type** whose
body can fall off the end without a value `return`, with a hint to add `return` or use a closure
`fn() -> T: <expr>` (whose body IS an expression and implicitly returns). The analysis
(`checker/mod.rs` `block_terminates`/`block_has_break`) is **sound/conservative** — never false-positives
on valid code: an `if`/`else` where every branch returns, an exhaustive `match` where every arm returns, a
`while true:` with no reachable `break`, and an `exit(...)` tail all count as terminating. A bare
`fn a(): 10` (no annotation → infers `nil`) and closures are **exempt**. `examples/edge_cases.chz`'s 6
inline non-void fns rewritten to multiline `return <expr>` (two-engine golden byte-identical). Docs:
`docs/syntax.md §5`, `docs/grammar.bnf` (comment), `gaps.md` (RESOLVED). All cargo wrapped at MemoryMax=6G;
full `cargo test` (2040) + `cargo test conformance` green, `cargo clippy --all-targets -- -D warnings`
clean.

**✅ Checker/semantics — inline-expr fn body implicitly returns + `nil` rejected in value position
(amends Option B).** Two coordinated changes, both two-engine (VM == interp) parity:
- **PART 1 — inline-expr body implicit return (Option A, inline-only).** A named fn written in the
  **inline** form (`fn a(): <expr>` on one line) whose single statement is a **bare expression** now
  **implicitly returns** that expression — exactly like a closure `fn(x): expr`. `fn a(): 10` returns
  `10` (inferred `-> int`); `fn dbl(x): x*2` works as a value / `.map` arg; `fn a() -> int: 10` is now
  **valid** (Option B's fall-off check is exempted for inline-expr bodies). A **multiline** 1-stmt body
  still does **not** implicitly return, and a declared-non-void multiline body still needs an explicit
  `return`. An inline **non-expression** statement (`fn a(): x = 5`) stays as-is (nil). The parser
  distinguishes the inline-expr body from a 1-stmt indented block (which `Block = Vec<Stmt>` otherwise
  erases) via a new `FnDecl.inline_expr_body` flag (`peek_at(1) != Newline` after the body colon +
  single `StmtKind::Expr`). The compiler (`compile_fn`) and interp (`call`) mirror `compile_closure`/
  `call_closure`: compile/eval the expr and Return its value. Return-type inference (`infer_fn_ret`) uses
  the inline expr's type as the inferred return.
- **PART 2 — `nil` used as a value is a type error.** A `Ty::Nil` (void) expression in **value
  position** — assignment RHS, a call/collection/tuple argument, a binary/unary operand, an index/range
  bound — now errors *"expression returns no value (nil) and cannot be used as a value"*, instead of
  silently propagating (`x := print(...)`, `print(log(...))`, `[log(...)]`, `1 + sort()`). A bare void
  call **as a statement** (`print("hi")` on its own line) stays legal, and returning `nil` from a fn
  (making it void) is **not** "using nil". Implemented as one `Checker::infer_value` helper routed
  through every value-position site (Let/Assign RHS, list/set/map/tuple/comprehension elements,
  `infer_binary`/`infer_unary`, `infer_index`/`infer_slice`, `expect_int`/`expect_bool`,
  `check_args_range`/`infer_all`/`one_arg`, and the builtin/constructor arg paths) — statement-position
  `infer` (`StmtKind::Expr`) and return-position `infer` (the inline-expr body, closure body) are left
  unchanged by design.
- Composition: `fn a(): print("x")` infers `-> nil` (a void fn, OK), but `y := a()` is then rejected.
  No grammar change (both reuse existing productions) → `cargo test conformance` stays green.
  `examples/inline_fn.chz` + `.expected` goldened (VM == interp). Docs: `docs/syntax.md §5`,
  `docs/grammar.bnf` (`<fnDecl>` comment), `gaps.md` (void-discard footgun → RESOLVED, cross-ref the
  bare-fn entry). NOTE (since 2026-06-21 superseded): string-interpolation operands ARE now checked —
  the `ExprKind::Str` arm parses `{…}` fragments and `infer_value`s each (see the soundness-fix entry
  below), so void-call / nil fragments are nil-banned too. All cargo
  wrapped at MemoryMax=6G; full `cargo test` (2104) + `cargo test conformance` green,
  `cargo clippy --all-targets -- -D warnings` clean.
- **Follow-up fixes (2026-06-17).** Two checker bugs in the inline-expr return path, both fixed:
  (1) an inline-expr body with a declared return type was type-inferred TWICE (statement-walk +
  return-assignability check), doubling every error inside the expr — `fn a() -> int: nope(5)` now
  reports exactly ONE diagnostic. The inline-expr stmt is now inferred once (the statement-walk is
  skipped for it). (2) the return-type assignability check was gated `if ret != Ty::Nil`, so a
  **non-nil** inline expr against an explicit `-> nil` was never validated — `fn a() -> nil: 10`
  type-checked clean but emitted `Return(10)` (a void fn returning an int). It is now rejected with the
  multiline path's wording *"function returns nothing, cannot return a value"*; a nil-typed inline expr
  against `-> nil` (a bare void call) stays legal. Tests: `inline_expr_error_reported_once`,
  `inline_nonnil_expr_against_nil_ret_rejected`.

**✅ Built-in conversions — str ↔ bytes (UTF-8) methods + `List()`/`Set()`/`Map()` constructors
(owner-requested; the natural follow-on to the just-landed `bytes`/`bytearray` types).** Two
conversion surfaces, mirroring the `bytes`/`bytearray` builtin-wiring exactly (3-engine parity), with
**no new syntax** — every form is an existing call/method production, so **`docs/grammar.bnf` is
intentionally UNCHANGED** (`cargo test conformance` stays green, proving no new terminal):

- **str ↔ bytes (UTF-8), as METHODS (not constructors — `bytes(x)`/`str(b)` names are already taken):**
  `str.encode() -> bytes` UTF-8-encodes (always succeeds — `str` is UTF-8 internally; copies the bytes
  out into a new immutable `bytes`). `bytes.decode() -> str` and `bytearray.decode() -> str` UTF-8-decode
  via `std::str::from_utf8`, mapping invalid UTF-8 to a **recoverable** `RuntimeError`
  (`"invalid UTF-8 in decode()"`, catchable by `recover:`, **never** a panic — same fault policy as the
  index/overflow faults). `"héllo".encode().decode() == "héllo"` round-trips a multi-byte char;
  `b"\xff\xfe".decode()` faults recoverably. **UTF-8 only** — no encoding-name argument (latin1/utf16 are
  an explicit future non-goal). Only `str` gets `.encode()`; only `bytes`/`bytearray` get `.decode()`.
  Wired through the method-dispatch path: checker `str_method_sig`/`bytearray_method_sig` + a new
  `bytes_method_sig` and a `Ty::Bytes` arm in `infer_method_call`; VM `core_method` Str arm +
  `bytearray_method` + a new `bytes_method` + an `Obj::Bytes` route in `do_method_call`, both decode
  paths sharing `Vm::decode_utf8`; interp `str_method` + `eval_bytearray_method` + a new
  `eval_bytes_method`, both sharing the free `decode_utf8` (error string byte-identical between engines).
- **`List(it)` / `Set(it)` / `Map(it)` constructors over ANY for-iterable** (NOT the narrow
  `Iterator[T]` protocol). Element types resolve through the checker's **`iter_elem`** — the single
  source of truth for "what `for x in X` accepts" — so `List([1,2])`, `List(myset)`, `List(b"hi")`,
  `List("ab")`, `List(range(3))`, `List(bytearray(..))`, and `List(myUserIterator)` all typecheck with no
  new protocol bound. `List(it) -> List[T]`; `Set(it) -> Set[T]` (the EXISTING `Set` broadened from
  list-only to any for-iterable, keeping the 0-arg empty-set form + the `Hashable` gate); `Map(it) ->
  Map[K, V]` where the element is **exactly a 2-tuple** `(K, V)` (a non-2-tuple is a **static** checker
  error), `K` `Hashable`, last-wins on dup keys (like the `{k: v}` literal). `list`/`map` are NEW reserved
  builtin names (added to `is_reserved_name` + both `is_builtin` sites + per-engine dispatch). The
  argument is **required** — an empty `list`/`map` is the `[]`/`{}` literal, so `List()`/`Map()` are
  checker errors pointing there. `Map(pairs)` (free call) and `xs.map(f)` (list HOF method) are separate
  namespaces — verified the parser routes them distinctly; documented in `docs/syntax.md`.
- **Runtime drain helper (the one genuinely new runtime piece).** Built-in collections copy elements
  directly (list/set elems, str→per-char `str`, bytes/bytearray→per-byte `int`, map→keys, range is
  already a materialized list). A user `next(self) -> Option[T]` struct (or a VM generator) is drained by
  looping its `next()` until `None`. **Interp:** extracted `drain_value_to_rows` from the post-eval body
  of `collect_iter_rows` (the for-loop's own materializer) — no duplicated iteration semantics; `set`
  rerouted through it, `list`/`map` added on `Interp::call`. **VM:** new `Vm::drain_iterable` (no runtime
  for-loop exists — it's fully compiled), driving user `.next()` via `run_proto`/`generator_next` with the
  growing accumulator + source **rooted on the operand stack** across every re-entrant call (GC-safe,
  copying the `builtin_set`/`list_hof`/`struct_hash` rooting pattern); `builtin_set` rerouted through it,
  `builtin_list`/`builtin_map` added to `do_builtin`.
- **Tests/golden:** checker `encode_decode_types` / `encode_only_on_str_decode_only_on_bytes` /
  `constructor_iter_types` / `list_zero_arg_rejected` / `map_requires_two_tuple` /
  `set_map_hashable_key_gate_preserved`; VM/interp parity `encode_decode_roundtrip_multibyte` /
  `bytearray_decode_matches_bytes` / `invalid_utf8_decode_recoverable` /
  `constructors_over_user_iterator_and_dupkey`; and `examples/conversions.chz` + `.expected` goldened on
  **VM + `--serial` + `--interp`** (byte-identical; uses a user `.next()` struct, NOT a generator, so all
  three engines agree). +7 tests (2036 green); `cargo test conformance` green (grammar unchanged); clippy
  clean. **Non-goals (stated):** non-UTF-8 codecs (latin1/utf16), base64/hex/sha (separate `std.*` gap),
  `tuple()` constructor (fixed-arity tuples can't be typed from a runtime-length iterable), `bool()`/
  truthiness (`if` stays strict-bool), and a formal user-visible `Iterable[T]` protocol (decoupled into
  its own future milestone — the constructors reuse the internal `iter_elem` union, not a new bound).

**✅ `bytearray` — mutable byte buffer (owner-requested; the second half of binary support — the
mutable sibling of `bytes`, Python `bytearray` / Go `[]byte` model — still a sequence, NOT a scalar).**
A heap byte buffer modeled on `list` (mutation flows through shared references), constructor-only
(no literal), mirroring the just-landed `bytes` variant-for-variant across the whole pipeline:

- **Constructor-only — no `ba"..."` literal** (the `b"..."` literal already owns `bytes`, so no lexer/
  parser/grammar change; `docs/grammar.bnf` is intentionally unchanged — a `bytearray(...)` call is the
  existing IDENT-LPAREN production). `bytearray` lexes as a plain identifier (guarded test). Four forms:
  `bytearray()` (empty), `bytearray(N)` (N zero bytes, Python; an absurd N faults **recoverably** via
  `try_reserve`, never a SIGABRT — same recoverable-fault invariant as `range()`/format-width), `bytearray(b)`/`bytearray(ba)` (mutable
  copy), `bytearray([ints])` (each 0–255). Both `bytes(...)` and `bytearray(...)` are NEW builtins (the
  `bytes` commit shipped no `bytes(...)` constructor — it was literal-only) — the **conversion bridge**:
  `bytes(ba)` snapshots, `bytearray(b)` copies.
- **Type `bytearray`** (`Ty::ByteArray`): `ba[i]`→`int`, **`ba[i] = x`** (`IndexSet`, M15 — the new
  capability `bytes` lacks; value 0–255 + index in range, else a recoverable fault), `ba[a:b:c]`→a new
  `bytearray`, `for x in ba`→`int`, `len`, `.push(int)` / `.pop()->Option[int]` / `.extend(bytes|
  bytearray|List[int])`, `==`/`!=` structural (incl. cross-type `bytes == bytearray` content-equal,
  Python parity). **NOT `Hashable`** (mutable ⇒ not a `map`/`set` key, the deliberate divergence from
  `bytes`, consistent with `list`). Sendable across the `--parallel` airlock by **deep copy** (like
  `list` — `WireValue::ByteArray` rebuilds a fresh independent buffer; no shared mutable view).
- **Runtime, BOTH engines (three-engine parity).** VM `Obj::ByteArray(Vec<u8>)` mutated IN PLACE
  through the `GcRef` heap slot (`heap.get_mut`), exactly like `Obj::List` — two bindings to the same
  `bytearray` observe each other's writes; interp `Value::ByteArray(Rc<RefCell<Vec<u8>>>)` interior-
  mutable like `Value::List` (deep-cloned ONLY across the airlock — a fresh `Rc<RefCell>`, NOT a cloned
  `Rc` like `Bytes`). Display/`str()`/interp = Python `bytearray(b'...')` repr via the shared helper
  `slice::bytearray_repr` (wraps `bytes_repr`), so all three engines are byte-identical by construction.
- **GC:** `Obj::ByteArray(Vec<u8>)` is a **LEAF** — raw `u8`, holds zero `GcRef`, so `children()` traces
  nothing (the difference vs `bytes` is the mutability of the slot, not GC reachability). `Vec<u8>` is
  24B (= `Obj::List`'s `Vec<Value>`), so the `Obj` size-cap (`size_of::<Obj>() == 88`) is unchanged.
- **Tests/golden:** `bare_bytearray_is_identifier` (lexer), `bytearray_*` (checker — incl. unhashable
  map/set-key rejection + conversion bridge), `vm_bytearray_*` + `bytearray_crosses_channel_deep_copy`
  (VM — incl. index WRITE, OOB/bad-value under `recover:`, shared mutation through two bindings,
  `--parallel` deep-copy independence), `interp_bytearray_*`, `bytearray_repr_wraps_bytes_repr` (slice),
  and `examples/bytearray.chz` + `.expected` goldened on **VM + `--serial` + `--interp` + `--parallel`**
  (byte-identical). +18 tests (2023 green); clippy clean. Remaining non-goals: a `byte`/`u8` scalar,
  non-UTF-8 codecs (latin1/utf16) + base64/hex/sha (a separate `std.*` gap), and byte-sequence methods
  beyond push/pop/extend/`decode` + the protocol ops. (UTF-8 `.decode()` has since **shipped** — see the
  conversions section above.)

**✅ `bytes` — immutable byte-sequence type (owner-requested; the Tier-A pre-JIT `Value`/`Obj`-variant
must-do from `gaps.md`, Python `bytes` model — NOT a new scalar).** A heap byte sequence threaded
through the existing `str`-shaped paths, reusing every protocol mechanism (no new ops/abstractions
beyond a `b"..."` literal + the const op):

- **Literal `b"..."` / `b'...'` (lexer-only, like the radix int literals).** `Token::Bytes(Vec<u8>)`;
  prefix fires ONLY when `b`/`B` is immediately followed by a quote (`b + 1` and `by` stay
  identifiers). Escapes: `\xHH` (exactly two hex digits → one byte 0x00–0xFF, the only way to write a
  byte ≥0x80) + `\n \t \r \\ \" \' \0`. **Rejects** `\u{…}` ("\\u not allowed in a byte literal") and a
  raw non-ASCII source char ("non-ASCII byte in byte literal"). Triple-quoted `b"""…"""` supported.
- **Type `bytes`** (`Ty::Bytes`): literal infers `bytes`; `b[i]`→`int` (Index protocol, M15), `b[a:b:c]`
  →`bytes` (Slice protocol over BYTE offsets, `src/slice.rs`), `for x in b` yields `int`, `len(b)` = byte
  count, `==`/`!=` structural, `Hashable` (valid `map`/`set` key). Immutable — `b[i]=x` is a type error
  (no `IndexSet`). Sendable (crosses the `--parallel` airlock by value, `WireValue::Bytes`).
- **Runtime, BOTH engines (three-engine parity is mandatory — this is a new feature landing on both,
  the sanctioned exception to "don't touch interp").** VM `Obj::Bytes(Box<[u8]>)` + `Op::ConstBytes`;
  interp `Value::Bytes(Rc<[u8]>)`. Index/slice/for/len/eq/ordering/hash/Display all reuse the existing
  dispatch with a Bytes arm next to the Str arm. **Display/`str()`/interp = Python `b'...'` repr** via
  ONE shared helper `slice::bytes_repr(&[u8])` called by both engines (parity by construction).
- **GC:** `Obj::Bytes` is a **LEAF** — it holds only raw `u8` (no `GcRef`), so `Heap::children()`
  returns nothing for it (marked reachable, traces no children, like `Str`/`Native`); the generic
  `alloc` path allocates it and `sweep` frees it via `Box<[u8]>`'s `Drop`. `Box<[u8]>` is 16B, so the
  `Obj` size-cap (`size_of::<Obj>() == 88`, `chzstr.rs` guard) is unchanged.
- **Tests/golden:** `byte_string_*` (lexer), `bytes_*` (checker), `vm_bytes_*` + `bytes_crosses_channel`
  (VM, incl. recover: + map key + `--parallel`), `interp_bytes_*`, `bytes_repr_python_style` (slice),
  and `examples/bytes.chz` + `.expected` goldened on **VM + `--serial` + `--interp`** (byte-identical).
  `docs/grammar.bnf` gained the `BYTES` primary terminal (`cargo test conformance` executes it; corpus
  `bytes_literal.chz`). +16 tests (1984 green); clippy clean.
- **Non-goals (v1):** `byte`/`u8` scalar, bignum, non-UTF-8 codecs (latin1/utf16) + base64/hex/sha
  (a separate `std.*` gap), a `{b:spec}` format-spec, and `ConstBytes` interning (allocs per push, like
  a list literal). (Two items once listed here as non-goals have since **shipped**: the mutable
  `bytearray` — see the `bytearray` section above — and UTF-8 `encode`/`decode` — see the conversions
  section above.)

**✅ Scoped enum variants — qualified-only `Enum.Variant` (owner-requested, explicit exception to the
M19/M18 feature freeze).** User-enum variants are now **scoped under their enum** and must be written
**qualified** (`Color.Red`, `Shape.Circle(2)`, `case Shape.Circle(r):`) in every position — value,
constructor, and `match` arm. A **bare** user-variant name is a hard compile error (the message names
the enum: *"'Red' is a variant of enum 'Color'; write it qualified as 'Color.Red'"*). Crucially, the
bare→binding trap is closed: a bare known-variant in a pattern errors instead of silently becoming a
catch-all binding. Because variants are keyed per-enum (`(enum, variant)`), **two enums may now share
a variant name** (`Color.Red` / `Light.Red` are distinct, with distinct dense `variant_id`s). The
**built-in** `Ok`/`Err`/`Some`/`None` (Result/Option) stay **bare** (they're special-cased, not in the
user registry); a user enum that reuses one of those names must qualify its own (`Signal.Err`), and a
bare `Err`/`Some` is always the built-in. The variant registry was re-keyed to `(enum, variant)` in
all three of checker / compiler / interp; the runtime layout is unchanged (the VM already matched on
the dense int `variant_id`). The interp's `try_bind` gained an enum check so a qualified pattern only
matches a value of that same enum (parity with the VM's int compare). `check_pattern_qualifier` also
rejects a qualifier that names the *wrong* enum (`case Light.Red:` over a `Color` scrutinee) — owning
the variant name isn't enough now that names are shared, else the dead arm would be miscounted toward
exhaustiveness and the real value would trap at runtime (regression test
`foreign_enum_qualifier_in_match_arm_is_rejected`). The parser's `[T](…)` type-arg
steal now also fires after `Enum.Variant`, so `Tree.Node[int](…)` works. **Both engines + parity**
(VM/`--serial`/interp byte-identical) via `examples/enum_qualified.chz`/`enum_layout.chz` + goldens +
`shared_variant_name_dispatches_per_enum`; conformance unchanged (semantics-only) plus a new
`tests/corpus/accept/enum_qualified.chz`.

**✅ M20 — In-language test framework (`assert` + `test fn` + `chezzi test`).** Chezzi now has a real
test facility. Three layers, all TDD'd:

- **`assert <cond>` / `assert <cond>, "<msg>"`** — a statement primitive that *faults with its source
  span* when `cond` is false (the headline need: which line failed). `cond` must be `bool`, `msg`
  (optional) `str` — checker-enforced. **Lands in BOTH engines** (parity discipline): the VM op
  `Op::Assert { has_msg }` and the interp `exec_stmt` arm produce a byte-identical message + span
  (default `"assertion failed"`); `examples/assert.chz` goldens this on both engines. Usable in plain
  `chezzi run`, independent of the runner.
- **`test fn` marker** — a `test` modifier before `fn`. A free `test fn` is an independent test; a
  `test fn name(self)` method makes its struct a **suite**. Compiler-*tagged* (`Proto::is_test`,
  `Program::tests`, `StructDef::test_methods`), so discovery is by tag, not a name scan (no
  silent-typo risk). Checker validates the shape: no params (free) / only `self` (method), returns
  nothing; a suite's name-matched lifecycle hook must be `fn name(self)` returning nothing.
- **`chezzi test [path]`** — a **Rust-side**, VM-only runner (forced: `recover:` only hands Chezzi the
  message, not the span, so only Rust catching `RuntimeError` gets `.span` for `file:line`). Collects
  `*_test.chz` files (single file or recursive dir walk; default cwd), compiles each as its own entry
  graph, runs the module top-level once, then invokes each tagged test on a reusable VM. Reports
  `PASS/FAIL name (file:line) msg` + a summary; non-zero exit on any failure. **Suites**: a synthetic
  `__new_<Suite>` thunk builds the instance once (reusing the struct-ctor compile path + default field
  exprs), then `before_all? → [before_each? → test → after_each?(always, like defer)]* → after_all?`,
  with a shared typed fixture (a default-initialized field mutated by hooks via mutable `self`).

Dogfood: `examples/{membership,operators,match_or,suite}_test.chz` author real tests with `assert`
(alongside the existing print-and-golden twins). Out of scope (deferred): `Span` file-id (an assert
faulting inside *imported* code reports the test file, not the library file — a documented MVP limit),
`assert_eq`/value-diff messages, parametrized-test sugar, a Chezzi-side runner, running the runner on
the interp engine. Grammar (`assertStmt`, `testFnDecl`) + corpus + `cargo test conformance` green.

**✅ M21 — Nominal `newtype`.** `newtype Name = <type>` (a new keyword, distinct from the transparent
`type` alias) is a DISTINCT nominal type wrapping the underlying — Go's defined-type model. It does
NOT silently mix with the raw underlying: a bare `int` is not assignable to a `UserId`, and a `UserId`
is not an `int`; only an explicit **construct** (`UserId(10)`, a call with one underlying-typed arg) or
**cast-unwrap** via the existing scalar casts (`int(uid)`/`float(m)`, and `str(n)` for a str-underlying)
crosses the boundary — no `.value`, no auto-deref. For a **scalar** underlying, same-newtype operators
**auto-flow** to the underlying's *native* op (unwrap→primitive-op→rewrap, NOT a user `add`):
`Meters + Meters -> Meters`, `Meters < Meters -> bool`, `==` compares inner; `Meters + float` /
`Meters + Seconds` are rejected (the whole point). A newtype carries its own (non-generic) methods and
satisfies protocols via them — `str(self)` (Stringable override), `hash(self)` (map/set key — opt-in,
*not* inherited), `compare`/`add` — and a numeric newtype satisfies `Add`/`Sub`/`Mul`/`Comparable`
intrinsically, so it flows into `fn twice[T: Add]`. Implemented by treating a newtype as ~a 1-field
nominal struct and reusing the struct/enum machinery at every layer: `Ty::NewType(key)` (checker),
`Obj::NewType{type_key,inner}` (VM) / `Value::NewType{type_key,inner}` (interp), `program.newtype_methods`
+ `newtype_home`, with `hash`/`str` dispatched **at runtime in both engines** (like the enum-hash fix)
and the wire/snap/airlock paths covered so a newtype is sendable iff its inner is. **Both engines +
parity** (VM/`--serial`/interp byte-identical) via `examples/newtype.chz` + `newtype.expected` golden;
new grammar `<newtypeDecl>` + `tests/corpus/accept/newtype.chz` + `cargo test conformance` green; clippy
clean; ~2347 tests pass. **v1 limits (documented):** an aggregate underlying (`newtype Names =
List[str]`) gets identity+construct+unwrap+own-methods ONLY — no `.push`/index/iterate forwarding;
no `derive`. Docs: `syntax.md §7`, `spec.md` (M21 row + enum-methods note de-staled), `grammar.bnf`.

**✅ M21+ — Generic newtypes (`newtype Stack[T] = List[T]`).** Type parameters on a `newtype`, the Go
defined-type model extended to generics — reuses the struct/enum generic plumbing end-to-end:
`type_params` on `StmtKind::NewType` (`parse_type_params`, the v1 hard-reject removed), a
`newtype_type_params` map mirroring `enum_type_params`, and `Ty::NewType(key, Vec<Ty>)` carrying the
instantiated args like `Ty::Enum`. The underlying + method signatures resolve `T` (hoist/body passes
`enter_type_params`); method dispatch substitutes the value's type args into the sig (`Stack[int].top()`
⇒ `Option[int]`); ctor infers args by unifying the underlying against the arg (`Stack([1,2])` ⇒
`Stack[int]`) with **turbofish** for the inference gap (`Stack[int]([])` — the empty `[]` can't bind
`T`, the documented `ConcurrentMap(RwShared({}))` case). **Methods-only:** a type-parameterized newtype
gets **no native operator auto-flow** — even `newtype Box[T] = T` over a numeric `T` — gated at every
auto-flow site (`Div`/`Mod`, `op_overload_result`, `ordering_allowed`, the `satisfies` intrinsic arm)
by a new `newtype_is_generic`; scalar `UserId=int`/`Meters=float` auto-flow is unchanged. **Cast-unwrap
propagates the instantiation** (the one genuinely new bit): `List(s)` for `s: Stack[int]` ⇒ `List[int]`
(via `newtype_unwrap_target` + a runtime peel in `builtin_list`/`set`/`map`, both engines — a
map-over-map yields the inner map directly). Runtime is **type-erased** (`Obj::NewType`/`Value::NewType`
carry no args), so generic instantiation / dispatch / hash / str are byte-identical across interp,
cooperative VM, and `--parallel` — golden `examples/newtype_generic.chz` + `.expected` is a standard
two-engine + `--parallel` test, no escape hatch. Cross-module via `NewTypeSigInfo.type_params`. Out of
scope (follow-up): static / associated methods (`Type.method()` / `T.zero()`). Docs: `syntax.md §7b`
(out-of-scope claim lifted → methods-only + turbofish), `spec.md` M21 row, `grammar.bnf` `<newtypeDecl>`.

**✅ Turbofish at the declaration site — type-side (PART 1).** Explicit type args for a generic are
pinned **at the site the generic is DECLARED**: declared on the type (`enum/struct/newtype [T]`) →
pinned **on the type** (`Box[int]`); declared on a member (`fn m[U]`) → on the member. For a generic
TYPE the args go ON THE TYPE, uniformly for enum **variant constructors** and **static methods**:
`Box[int].Has(5)`, `Result[int, str].Ok(5)`, nullary value `Box[int].Empty`, generic static
`Box[int].empty()`. Multi-param types use the comma form (`Result[int, str].Ok`). The OLD **gliding**
form `Enum.Variant[T](args)` (type args on the variant) is **removed** — the checker emits a redirect
(`put the type arguments on the type: Box[int].Full(...)`); the bare/module-qualified variant branches
both guard it. **Parser:** the SINGLE-arg head (`Box[int].member`) stays on the index path (the parser
can't tell it from `arr[i].field`), reinterpreted by the checker; the MULTI-arg head commits a new
`ExprKind::TypeApply{name, args: Vec<Type>}` carrier (the disambiguating comma — a comma in a subscript
is otherwise always a parse error, so it steals nothing) parsed via `try_parse_type_apply`. **Checker:**
one `type_apply_head` helper resolves both carriers to `(type-name, [Type])`; in `infer_call` it is
**variant-first** (`infer_variant_call` with the resolved targs seeded — arity-checked by
`seed_targs`), else `infer_static_call`; `infer_field` gains the nullary-value branch returning the
**resolved** type args (not `Unknown`). The single-`Index` path also gained the variant-first check
(a gap the previous static-methods work left). **Compiler + interp** get matching `type_apply_head_name`
branches emitting the same `Op::NewEnum`/`Op::CallStatic` as the bare forms (runtime is type-erased).
**PART 2 (now landed, below).** **Both engines + `--parallel`** byte-identical via golden
`examples/turbofish_type_args.chz` + `.expected` (the test also asserts the program type-checks clean);
checker unit tests for each rule (single/multi-arg variant, seeded-not-Unknown, arity mismatch, nullary,
old-form redirect, static regression); a parser unit test; a `tests/corpus/accept` file for the
differential conformance check; clippy clean. Migrated the one surface use `examples/explicit_type_args.chz`
(`Box.Full[int](9)` → `Box[int].Full(9)`). Docs: `syntax.md` (§7a generic-static + enum/variant
sections — the declaration-site rule; multi-arg lifted), `spec.md` (new milestone note + static-method
single-arg limit de-staled), `grammar.bnf` (the `<typeApply>` head + `Type[T…].member` postfix
productions; old gliding production removed from prose).

**✅ Turbofish at the declaration site — member-side (PART 2).** Completes the declaration-site rule: a
**member** declares its OWN type args (`fn make[U]`, `fn first[A, B](self, …)`), pinned on the member
and composing with PART 1's type-side args. `Box[int].make[str](x)` supplies the enclosing `T` AND the
method `U`; `Box.make[str]("hi")` / `s.first[int, str](1, "x")` are bare carriers; inference is the
default (`Box[int].make(5)` ⇒ `U = int`). **Checker:** `infer_static_call` gained an `mtargs` arg and now
builds ONE by-name substitution map over BOTH the enclosing type params (seeded from the type turbofish)
and the method's own `[U]` (seeded from `mtargs`), inferring the rest from the args and degrading EVERY
un-inferred param — enclosing or method — to `Ty::Unknown` (no leaked `Ty::Param`; mirrors the static
fix at 7c75ab2). **UPDATE — parser steal BROADENED (uniform-receiver rule).** The member-turbofish steal
now fires on **ANY** `Field` receiver, not just a `Field` over a bare ident: `recv.name[X](args)` parses
as a method turbofish on a bare ident, a call result (`W(1).cast[str]("a")`), a field (`h.w.cast[U](x)`),
or an index (`xs[0].cast[U](x)`). `try_parse_type_arg_call` stays speculative (commits only on the
`[ <typeList> ] (` shape, else restores pos+depth), so `obj.items[0]`/`m.data[k]` (no call) and the
numeric `arr[0].handlers[0](20)` still backtrack to index-then-call. The combined `Box[int].make[str](x)`
now also rides the Field-callee path (the receiver `Box[int]` is itself a postfix) and is dispatched by
the `type_apply_head` branch — threading **both** the enclosing type args (`[int]`) and the method targ
(`[str]`, was dropped as `&[]`, now `&targs`); a method turbofish on a generic **variant** ctor
(`Box[int].Has[str](5)`) is now explicitly an error (the old Index-over-Field block that caught it is
bypassed). **AUTHORIZED REGRESSION (accepted, documented):** index-then-call of a fn-**valued** field on
a non-bare receiver — `arr[i].handlers[k](10)` — now parses as a turbofish and errors; workaround is
parens `(arr[i].handlers[k])(10)`. This makes non-bare receivers UNIFORM with the bare-ident case
`w.handlers[k](10)`, which already required parens. `infer_method_call` gained a `type_args` arg threaded into `infer_generic_method`
(instance multi-turbofish `s.m[A, B](x, y)` now seeds + arity-checks + catches an explicit-targ/arg
conflict, previously silently dropped) plus a top-of-fn guard — BEFORE the `.iter` fast-path — rejecting a
member-level turbofish on a builtin/non-generic member (fixes the `.iter[int]()` swallow; `len[int]()`
already errored). The `fn_sig` shadow guard already fires for static methods. **Compiler + interp** get
matching combined-Index-callee arms (peel the erased index → same `Op::NewEnum`/`Op::CallStatic` /
`build_variant`/`call` as the bare forms; runtime is type-erased). **OUT OF SCOPE (unchanged):** static
methods on `newtype`; associated protocol requirements (`T.zero()`) — **SHELVED** after two rejected
attempts, see `docs/future.md` §3.13; protocols stay instance-only.
**Both engines + `--parallel`** byte-identical via golden `examples/turbofish_member_args.chz` +
`.expected` (asserts type-checks clean too) incl. the regression-guard shape; new checker unit tests
(static own-`[U]` inferred, no-leak degrade, combined ok + mismatch, shadow-static rejected,
`iter[int]()` errors, instance multi-turbofish ok + mismatch, index-then-call regression);
`cargo test conformance` re-run after generalizing the `grammar.bnf` method-turbofish production to
`<typeList>`/`<argList>`; clippy clean. Docs: `syntax.md` §7a (member-level + combined + by-name unified
substitution; removed the "cannot declare its own `[U]`" / "method-level turbofish reserved" notes),
`spec.md` (PART 2 milestone note; lifted the static-own-`[U]` limit), `grammar.bnf` (generalized
production + combined-form checker-reinterpreted comment).
**KNOWN FOLLOW-UP (deferred, doc-only — revisit later):** the authorized-regression error for
`recv.name[k](args)` where `name` is a fn-valued field / not a generic method is currently the bare
`method '…' takes no type argument(s)`. Upgrade it to a *guiding* diagnostic that detects the
fn-field/non-generic-member case and suggests the parens workaround `(recv.name[k])(args)` so users
hit by the uniform-steal rule are pointed at the fix without reading the spec caveat. Checker-side,
low risk, no parser change; pairs with a regression test on the parenthesized form.

**✅ Static (associated) methods on struct + enum — the "no self ⇒ static" rule.** A struct/enum
method whose first parameter is **not** `self` (or which has no parameters) is a **static** method,
called `Type.method(args)` instead of `value.method(args)` (the Rust `fn new` ergonomic). **Additive**
— the positional `Name(...)` ctor is unchanged; static methods unlock named/alternative ctors
(`Rect.square(5)`) and validating ctors returning `Result`/`Option` (`Email.parse(s) ->
Result[Email, str]`, `Color.from_str(s) -> Option[Color]`). Instance vs static are **different call
shapes** — neither is invocable as the other (clear errors pointing at the right form). **Note — a
behavior change:** a method like `fn getx(p: Point)` (first param not `self`) is now STATIC, not an
instance method with a positionally-bound receiver (the old "receiver is positional, any name"
convention is gone). Classification is a pure decision over the existing AST (`first param != "self"`)
threaded through all three engines: a new `FnSig.is_static` (checker), a `Compiler.static_methods`
set populated in `hoist_types`, and `is_static_method()` in interp — so the engines agree by
construction. **Resolution** mirrors the existing `Enum.Variant(args)` qualified-ctor branch in
`infer_call`/`compile_call`/`eval_call`: a new static-method branch alongside the variant check (for
enums the **variant wins first**; variant/static names must be **disjoint**, a new decl-time check).
New `Op::CallStatic{type_key, method, argc}` (separate variant, mirrored in interp) executes like the
enum-method slow path **minus the receiver** (`do_static_call`, `arity == argc`, `push_frame_in_place`,
generator edge via `alloc_generator`). **Generic statics** via the **type-level turbofish**
`Box[int].empty()` (reinterprets `Field{obj: Index{Ident, idx}, name}` — indexing a bare type is
otherwise invalid, so unambiguous). (Multi type-arg + variant-side resolution were generalized by the
later "Turbofish at the declaration site — type-side" milestone above; a static method declaring its
own `[U]` + the member-level turbofish landed in the "member-side (PART 2)" milestone above.) v1 limits
(documented): static methods do **not** participate in
**protocol** conformance (instance-only); static methods on `newtype` are a follow-up (the newtype
receiver-error site stays). **Both engines + `--parallel`** byte-identical via golden
`examples/static_methods.chz` + `.expected` (mirrors `newtype.chz`); checker unit tests for each rule
+ the negative cases; clippy clean. Docs: `syntax.md §7a`, `spec.md` (M21 newtype-static note
de-staled + a new "Static methods" milestone note), `grammar.bnf` (`Type.method` / `Type[t].method`
postfix forms documented — no new production).

**✅ Raw string literals — `r"…"` / `r'…'` / triple `r"""…"""` (and uppercase `R`).** A verbatim `str`:
**NO interpolation** (braces `{`/`}` are literal — `r"{}"` prints `{}`, no `{{}}` doubling) and **NO
escape processing** (`r"\d+"` is literal backslashes — best for regex / Windows paths / brace-heavy
JSON). The escape hatch for the always-on `{…}` interpolation. Type is plain `str` (`Ty::Str`),
identical downstream. Lexer-only: a new `Token::RawStr` → distinct `ExprKind::RawStr` (mirrors
`Bytes` across all 9 touch-sites) so Rust's exhaustiveness checker FORCES both engines to handle it —
the VM emits `Op::ConstStr` directly and interp returns `Value::Str` directly, **both bypassing
interpolation**, so VM/interp/`--serial` are byte-identical by construction. The `r`/`R` prefix fires
only when immediately followed by a quote (adjacency rule — a variable named `r` is unaffected,
exactly like `b`). Short form can't contain its own quote; triple form embeds quotes (JSON).
**Two-engine parity** golden `examples/raw_string.chz` + `.expected`; `tests/corpus/accept/raw_string_literal.chz`
+ new `RAWSTR` terminal in `grammar.bnf <primary>`, `cargo test conformance` green; clippy clean.
**Out of scope (follow-ups):** combined raw-bytes `rb"…"`/`br"…"`, Rust-style `r#"…"#` hash delimiters
(the triple form already embeds quotes). Docs: `syntax.md §2/§10`, `spec.md`, `grammar.bnf`.

**🟦 M19 — Perf track (in progress).** M19 is a **pre-JIT perf push**, not a feature freeze — language
work still lands (e.g. module-scoped types, 2026-06). This milestone is otherwise pure
optimization, so the bar is **behavior-preserving + two-engine parity** on every change. Measure first
(`cargo run --release -- run benches/run.chz`), land behind a failing-then-green correctness test, keep
parity green, re-measure, record the delta in [`docs/benchmarks.md`](docs/benchmarks.md). Several levers
moved a *different* bench than predicted — trust the measurement, not the a-priori guess. The frozen
interp is untouched by VM-only work, so parity is automatic for those changes.

**Slice syntax → Python colon (owner-requested language change, mid-M19).** The subscript-slice form
moved from Rust-range `xs[a..b]` to Python `xs[a:b]` with the full surface: open bounds (`xs[1:]`,
`xs[:3]`, `xs[:]`), step (`xs[a:b:c]`), reverse (`xs[::-1]`), and **negative indexing** (`xs[-1]`,
`xs[-2:]`) on plain index AND slice bounds, for `list`/`str` and as an assignment target (`xs[-1] = v`).
Out-of-range rule = Python's asymmetry: a plain `xs[-100]` **faults** (`index -100 out of bounds (len N)`),
a slice bound `xs[-100:]` **clamps**. The `..` operator is unchanged — it stays the for-loop / match-pattern
range. The parser owns the colon (`parser::parse_subscript`, replacing the old post-hoc Range→Slice rewrite);
`ExprKind::Slice` now carries `start/end/step: Option<Box<Expr>>`. Runtime is a single shared resolver
(`src/slice.rs`: `slice_indices` + `norm_index`, derived from CPython `slice.indices`) called byte-identically
by both engines — it replaced the duplicated `clamp_range`. User `Slice` structs get the full surface via
default params: `slice(self, start: int?=None, end: int?=None, step: int?=None) -> R` (the runtime passes
real `Option[int]` components). Strict TDD, both-engine parity green, `examples/slicing.chz` +
`examples/edge_cases.chz` + `std/str.chz` migrated, `docs/grammar.bnf` colon-slice rule + `cargo test
conformance` green.

**Landed phases** (all TDD'd, two-engine-parity-clean; numbers + per-lever notes in
[`docs/benchmarks.md`](docs/benchmarks.md), ranked backlog in [`docs/future.md §4`](docs/future.md)):

- **Phase 1** — killed the per-call `Obj` clone in `invoke_value`; jump-relocating peephole + constant
  fold (`src/compiler/peephole.rs`, replicating the VM's checked overflow/div-by-zero semantics);
  superinstructions (`Op::BinLocalLocal`/`BinLocalConst`/`IncLocal`) fusing the hot local/const arith
  windows with an exact unfused fallback.
- **Phase 2** — in-place call args (`do_call` runs over the args already on the stack, killing the
  per-call `split_off` `Vec`); `stringify`-into-buffer (`BuildStr` reuses one buffer across interpolation
  parts).
- **Phase 2b** — global-slotting: every module global gets a stable `u32` slot; `GetGlobalSlot`/
  `SetGlobalSlot`/`DefineGlobalSlot` index `Obj::Module.slots` with no hash. Slot map lives in the shared
  `Arc<Program>` so parent and faulted-worker agree by construction (removes a latent snapshot
  ordering fragility).
- **Phase 3** — `ConstStr` interning (per-heap cache keyed by the literal's data pointer, GC-rooted,
  swapped with the heap across `swap_ctx`); per-char single-alloc `alloc_char` at every 1-char-string
  site.
- **Phase 4** — struct-field inline cache: `GetField`/`SetField` carry a per-call-site IC id into a
  per-`Vm` `field_ic` caching the field index. Runtime IC (the compiler is type-erased); holds an index
  not a `GcRef`, so it's invisible to GC/snapshots/`swap_ctx` and every access self-verifies.
- **Phase 5a** — FxHash (`src/vm/fxhash.rs`, no new dep) for `MapData`/`SetData` index + `str_intern`.
  `values_equal` confirms every hit ⇒ behavior-preserving. (Footgun caught by measuring: a naive
  multiply-only FxHash was 100× slower on int keys — fixed with a splitmix64 finalizer.)
- **Phase 5b** — struct type-id guard (`Obj::Struct.tid`, dense layout id): the field-IC hit guards on
  `cell.tid == obj.tid` instead of a string re-verify. Measured **neutral**, kept as the principled
  guard. The field-IC lever is now spent.
- **Call-loop flattening** — the bytecode `Op::Call` fast path now pushes the callee frame and lets the
  running `run_until` loop execute it (CPython-3.11 "zero-cost frames"), removing the per-call Rust
  `run_until` recursion **and** the per-call `Arc::clone(&self.program)`. HOFs / struct methods keep the
  re-entrant `run_proto` (they need the callee result synchronously mid-Rust-method). **Robustness bonus:**
  deep *plain* recursion no longer consumes host stack — bounded by `MAX_CALL_DEPTH`, not the thread
  stack. (Follow-up: flatten `do_method_call` for the `struct`/method benches.)
- **Small-string optimization (SSO)** — `Obj::Str` holds a `ChzStr` (`src/vm/chzstr.rs`): ≤22 UTF-8
  bytes live inline in the variant, longer spill to `Box<str>`. `Deref<str>` + `From` impls kept the
  ~100 match arms unchanged; `Clone`/`Eq`/`Hash` delegate to `as_str()` so map keys / interning / `==`
  stay byte-identical. `size_of::<Obj>()` unchanged at 88 B (guard-tested). Closes the SSO lever.
- **Phase 6 — method-call IC + flatten `do_method_call`** — `Op::CallMethod` carries a per-site `ic`;
  a struct receiver caches `(tid → proto, module_idx)` in a per-`Vm` `method_ic` vec (a hit skips the
  `program.structs` clone + the name-keyed `def.methods` probe), AND flattens the call (frame pushed in
  place; the running `run_until` executes it, no re-entrant `run_proto`). No `GcRef` in the cell ⇒
  swap/GC-invisible like the field IC; `NO_IC` re-entry callers (`spawn`/`defer` method) keep `run_proto`.
  **`struct` 2.90×→2.63× (−9%)**, the predicted bench; only it moved (it's the OO-dispatch bench).
- **Phase 7 — inline hot ops in `run_until`** — the dispatch loop handles the hottest opcodes inline
  (`GetLocal`/`SetLocal`, the superinstrs, `Jump`/`JumpIfFalse`, `Call`/`Return`) and delegates the tail
  to `step`, skipping a fn-call + the big match jump-table per op. Inlined arms reuse `step`'s helpers /
  copy its 1–3-line bodies (one source of truth). **Biggest lever of the session — moved every op-bound
  bench: `loop` 1.30×→~1.10× (−15%, was the dispatch floor), `list` 3.06×→~2.55× (−17%), `primes` −8%,
  `fib` −6%, `struct`/`str`/`map` −4–5%.**
- **Phase 8 — call-site spec for `Op::Call` — analyzed, DEFERRED (no-gain).** After Phase 7 inline,
  `do_call`'s happy path is already lean (the deref a call-IC skips is ~2–3 instrs); fib's residual is
  frame-setup in `finish_frame`, which a dispatch cache doesn't touch. A correct call-IC also can't avoid
  a heap-specific callee handle ⇒ `swap_ctx` hazard for ~0 gain. fib's real lever is Tier 2 (PEP 659) /
  Tier 3 (JIT). Full rationale in [`docs/benchmarks.md`](docs/benchmarks.md).
- **Memory layout #3 — positional closure captures.** `Obj::Closure.captured` moved from a per-closure
  `HashMap<String, Value>` to a positional `Vec<Value>` indexed by a compile-time slot; `Op::GetCaptured`
  carries a `u32` slot (hash-free `captured[slot]` hot read, no string hash) instead of a name; capture
  names live in `Proto.capture_names` (cold path only: the home-global fallback, error messages, and
  wire/snap name carrying). Nested captures (a closure capturing an enclosing closure's capture) map by
  `CapSrc::Captured(parent_slot)` stamped at compile time. Behavior-preserving + **three-engine parity**
  (`examples/closure_capture.chz` on VM/interp/--parallel). **−45% (1.83×)** on a closure
  construct+capture-read micro (`benches/chz/closure.chz`); standard suite neutral (no closure-heavy
  bench). `Obj::Closure` shrank 88→64 B (Module still caps `Obj` at 88 B, guard intact). JIT groundwork:
  constant capture offsets for the future Cranelift codegen. (Memory layout land order **#1 ✅ → #3 ✅ →
  #2 ✅**; see `docs/future.md` §4.)
- **Memory layout #2 — enum `variant_id` (completes the #1→#3→#2 sequence).** `Obj::Enum` dropped its two
  per-instance `Box<str>` (the type name + variant name, both program-global) for a single dense
  `variant_id: u32` — the enum analogue of struct `tid`. Match-arm dispatch, `==`, and `?` are now
  pure-int compares (was variant-name string compares / `ty==ty && variant==variant`); the type + variant
  names resolve from a new `Program::variants_by_id` table on the cold path only (Display/stringify/
  error/wire/snap). Native `Ok`/`Err`/`Some`/`None` hold the **reserved** fixed ids
  `VID_OK`(0)/`VID_ERR`(1)/`VID_SOME`(2)/`VID_NONE_VARIANT`(3); user variants follow at `4..`, so the
  reserved range is **disjoint** from every user id. `?`/top-level-error gate on the constants, and the
  native construction path (`alloc_enum`) stamps the constant **directly** (never a `variants[name]`
  lookup) — so a user enum may shadow a native name (`enum Foo: Some(int)`, allowed) without a genuine
  native Option/Result being stamped with the user's id. `Op::NewEnum`/`Op::MatchArm` carry the
  compile-time id; wire/snap carry the dense `variant_id` **directly** (shared `Arc<Program>` ⇒ meaningful
  both sides; preserves identity under shadowing). *(Parity bug fixed 2026-06-16: the first cut
  name-resolved native construction, so a user enum shadowing `Some`/`Ok`/… collapsed native-vs-user `==`
  and broke `?` — a VM-vs-interp divergence. Now guarded by two shadow regression tests + a shadowing
  section in the golden example.)*
  Behavior-preserving + **three-engine parity** (`examples/enum_layout.chz` on VM/interp/--parallel).
  **−20% (1.25×)** on an enum construct+match-dispatch micro (`benches/chz/enum.chz`); standard suite
  neutral. `Obj::Enum` shrank 56→32 B (Module still caps `Obj` at 88 B, guard intact). JIT groundwork:
  numeric variant id → constant/jump-table dispatch for the future Cranelift codegen + match-on-enum.
- **Memory layout #4 — box `Obj::Module`.** The fat `Module { name, slots, index }` variant (88 B —
  `Box<str>`16 + `Vec`24 + `HashMap`48) was the sole thing capping `size_of::<Obj>()` at 88 B, forcing
  every heap `Slot` to 96 B even though modules are rare + cold (a handful per program). Boxed its
  payload behind `Obj::Module(Box<ModuleData>)` (mirrors the already-boxed `Obj::Generator(Box<…>)`),
  so `Module` is now 8 B and **`size_of::<Obj>()` drops 88→64 B** — capped now by `MapData`/`SetData`
  (56 B payload + 8 B discriminant). ~one cold pointer hop off the module-member path; every heap object
  shrinks. Mechanical VM-only change (checker never names `Obj`); GC `children()` still traces
  `m.slots`, `live_bytes` still counts `m.slots.capacity()*size_of::<Value>()`. Behavior-preserving +
  serial==M:N parity (full `cargo test` incl. `chz_suite_passes_both_engines`); guard-pinned at 64 B
  (`heap.rs` + `chzstr.rs`). RSS delta measured post-merge.

- **Memory layout #5 — inline small struct `fields`.** `Obj::Struct.fields` was a `Vec<Value>` — a
  SEPARATE heap malloc per struct instance (2M structs = 2M small buffers, ~61MB RSS on
  `benches/chz/many_struct.chz`). Replaced with a hand-rolled `Fields` enum in `heap.rs`:
  `Inline { len: u8, vals: [Value; 3] }` folds ≤3 fields (the vast majority) into the 64B `Obj` slot —
  **zero second malloc** — while `>3` spill to `Spill(Box<[Value]>)` (exact-length, no `Vec` capacity
  slack). Fields are FIXED at construction (positional hidden-class layout; no `.push/insert/resize`
  growth sites), so an inline-or-spill repr with no growth is safe. No new dep (no `smallvec`); `Obj`
  stays 64B (`size_of::<Fields>() == 32`, guard-pinned `fields_inline_width_fits`). `Fields` exposes a
  `Vec`-compatible surface (`from_vec`/`len`/`as_slice`/`as_mut_slice`/`iter`/`get`/`get_mut`/`heap_bytes`
  + `Index`/`IndexMut`) so the ~16 `Obj::Struct` touch sites changed minimally; the field IC's
  `get`/`get_mut` hot paths and in-place `s.a = s.b` writes are byte-identical (write-through
  `as_mut_slice`). GC `children()` still traces every field (unused `Inline` nil slots yield no gcref);
  `live_bytes` counts `Inline`→0, `Spill`→`len*size_of::<Value>()` via `Fields::heap_bytes`. Mechanical
  VM-only change (checker never names `Obj`); behavior-preserving + serial==M:N parity (full `cargo test`
  incl. `chz_suite_passes_both_engines` + `conformance`). RSS delta measured post-merge.

- **Memory layout #6 — GC mark bit → parallel bitset.** `Slot` was `{ obj: Option<Obj>, mark: bool }`
  — `Option<Obj>` is already 64B (`Obj`'s spare-discriminant niche makes `None` free), so the `mark: bool`
  was pure padding pushing the slot to **72B**, and every mark/sweep scan pulled the full 64B `Obj` into
  cache just to touch 1 bit. Dropped the field (`Slot { obj: Option<Obj> }`, guard-pinned
  `slot_element_is_64b` = 64B) and moved the mark to a dense `marks: Vec<u64>` bitset on `Heap` (bit
  `i&63` of word `i>>6`), grown in lockstep with `slots` at the new-slot alloc arm. Three one-line
  helpers `is_marked`/`set_mark`/`clear_mark`; `mark()`/`sweep()` rewired to the bitset, reproducing the
  EXACT mark-then-sweep-and-clear protocol (survivors cleared in the sweep pass, holes never marked →
  post-sweep invariant: all bits 0). Saves the 8B mark padding per slot (≈16MB on the 2M `many_struct`
  bench) and lets the sweep mark-scan iterate a compact bit array instead of touching each payload.
  `src/vm/heap.rs` GC-internal only — no `Obj`/`Fields`/checker/observable change; behavior-preserving +
  serial==M:N parity (full `cargo test` incl. `chz_suite_passes_both_engines` + `conformance`, all
  GC-stress rooting green), clippy clean. RSS delta measured post-merge.

- **Memory layout #7 — drop `Obj::Struct.name: Box<str>`, resolve the type name from `tid`.** Every
  struct instance carried a per-instance `name: Box<str>` (the type IDENTITY KEY, e.g. `<main>::Point`)
  allocated fresh at construction — a **second heap alloc per struct** on top of the slot, ~28% of RSS on
  the 2M-struct `benches/chz/many_struct.chz` (probe: nulling the name alloc took `many_struct`
  205.6→148.9 MB). It was redundant: `tid: u32` already identifies the type. Mirrored the shipped enum
  lever (`variant_id`): dropped the field (`Obj::Struct { tid, fields }`), added a dense reverse index
  `Program::struct_names: Vec<Box<str>>` (`struct_names[tid]` ⇒ the `structs` map key, built once at
  program construction from `program.structs` via `rebuild_struct_names`), and a resolver
  `Vm::struct_name_of_tid(tid) -> &str` (O(1) index, mirrors `enum_names`). The ~14 name-read sites
  resolve from `tid` on the cold path (method dispatch / Display / stringify / arith-overload / hash /
  wire / snap); the warm method-dispatch (`call.rs`) + overload (`arith.rs`) paths index `struct_names`
  in O(1) (never a scan). Struct **equality** now compares `tid` (same tid ⇒ same StructDef ⇒ identical
  field order — one int compare, subsuming the old name compare, exactly as enum equality compares
  `variant_id`). Wire/snap format UNCHANGED: `WireValue::Struct`/`SnapValue::Struct` still carry the name
  string (resolved from `tid` at the send site, receiver re-derives `tid` via `struct_tid`) → byte-
  identical cross-worker crossing, workers share `Arc<Program>` so `tid` is stable. `Obj` stays 64B
  (`MapData`/`SetData` still cap the payload at 56; guard-pinned `obj_iter_within_size_cap`). Mechanical
  VM-only change (checker never names `Obj`); behavior-preserving + serial==M:N parity (full `cargo test`
  incl. `chz_suite_passes_both_engines` + `conformance` + all `*_gc_stress`), clippy clean. RSS delta
  measured post-merge.

**Remaining / blocked levers:**

- **NaN-boxing `Value` is BLOCKED by full 64-bit ints, not "next."** `Value::Int` is a full `i64`; an
  i64 + a type tag don't fit in 8 bytes alongside `f64`, so it needs boxed big ints (branch + alloc per
  int, semantics-sensitive overflow) — not behavior-preserving, uncertain win on the very int benches it
  targets (Lua 5.4 stayed 16-byte for this exact reason). Blast radius is VM-only (the frozen interp has
  its own `Rc`-based `Value`), but it's a milestone spike. Parked.
  - **UPDATE 2026-07-18 — reopened as the 8B-`Value` int-favoring pointer-tag milestone (NOT NaN-box).**
    Plan `~/.claude/plans/2026-07-18-8b-value-pointer-tag-*.md`: inline-tag `Int` (`(n<<1)|1`, ±2^62), box
    the rare wide int + every `f64`. **Phase 0 (heap-side scaffolding) landed** — additive, parity-trivial:
    (1) `Heap::live_bytes()` + a peak high-water probe reported behind env `CHEZZI_HEAP_STATS=1` as
    `[heap-stats] peak_live_bytes=<n> size_of_value=<n>` (baseline `benches/run.chz` = peak_live_bytes 24277
    at size_of_value 16); (2) two GC-leaf `Obj` variants `BigInt(i64)`/`FloatBox(f64)`, unused by real
    programs (reachable only from a unit test), each behaving identically to the inline `Int`/`Float` for
    display/hash/eq/order/wire; `size_of::<Obj>()` stays 88. Phase 1 (the `struct Value(u64)` swap) is gated
    on the measured `peak_live_bytes` drop vs this baseline.
  - **DONE 2026-07-18 — 8-byte `Value` MERGED to main (`fa3c014`); measure gate passed.** `Value` is now an 8-byte `struct Value(u64)`
    (`assert_eq!(size_of::<Value>(), 8)`): bit0=1 → inline `Int` `(n<<1)|1` (±2^62); low3 `000` → `Obj`
    (incl. boxed `BigInt`), `010` → `Float` (its own tag → `is_float` is heap-free, points at
    `Obj::FloatBox`), `100` → the `Nil`/`False`/`True` immediates. Wide ints and every float now box via
    `Vm::make_int`/`Vm::box_float` and read back via `Vm::int_of`/`Vm::float_of`; classification goes
    through `Value::view()` → `ValueView` (a boxed float/big-int surfaces as `Obj(gcref)`, resolved
    heap-side). **Behavior-preserving**: overflow still faults at the i64 ceiling (boxing only lifts the
    *inline* ceiling to ±2^62, not the i64 one), int `==`/order stay exact-i64, `1 == 1.0` still true,
    two independently-boxed equal floats compare `==` and hash equal. GC traces boxed floats via
    `Value::child_gcref` (both the Obj and Float tags) at every root/children site; the airlock re-boxes
    on the destination heap in `from_wire` identically on both engines. Two-engine parity + difftest +
    conformance green. Observable limit lifted (design §3): `[x] * n`'s `count * size_of::<Value>() ≤
    isize::MAX` bound doubled (Value 16B→8B), so a marginally larger repeat now succeeds. **Measure gate
    (same machine/session, 16B→8B): the dispatch-floor benches got FASTER — `loop` 1.13×→1.03× CPython
    (near parity), `fib` 3.29×→2.95× (first sub-3×), `map`→1.77×, `poly_method`→3.94×; only `primes`
    +2.7% (in-noise). Cache-density win beat the tag decode tax.** Full numbers in `docs/benchmarks.md`.
    Prereq soundness fix (`ccbd3c4`): int `==` was lossy `as_f64` above 2^53 → now exact i64. Float-const
    interning (plan Task 5) DEFERRED — no measured float regression on the int-heavy bench set.
- **String concat/split builder/rope** moves no current bench — `join` already buffers into one `String`;
  `+`/`split` aren't exercised by the `str` bench.
- **Arith specialization + frame pooling: effectively closed** — superinstructions inline the monomorphic
  int path; `CallFrame`'s `Vec`s are alloc-free (no per-call frame alloc to pool).
- **Big/separate milestones** (later-stage, once the language has matured): NaN-boxing as its own
  milestone, register VM, generational/incremental GC, and **Cranelift AOT/JIT as the stretch end-game**.

Gap to CPython after Phases 6–7 **~1.1×–3.2×** slower (worst still call-bound `fib` ~3.2×, then `map`/
`struct`/`list`/`primes` ~2.3–2.7×, `str` ~2.0×; **`loop` ~1.1×** — near parity, was the dispatch
floor), startup ~11× **faster**. **1607 tests** green, conformance 7/7, `clippy --all-targets` clean.

**Tier-2 index specialization landed (2026-06-12):** Int-key fast path in `get_index`/`set_index`
(skips `hash_key_rooted`'s rooting — alloc-free for an int key) + inline `GetIndex`/`SetIndex` in the
`run_until` hot arm. **`list` −4%** (its `for x in xs` lowers to per-element `GetIndex`); **`map`
neutral** (FxHashMap-probe-bound, not rooting/dispatch-bound — the predicted target didn't move, the
recurring "measure, don't guess" lesson). Behavior-preserving (7 `idxspec_*` VM==interp guards, incl.
the Int/Float key-collision trap). Moving `map` needs a denser int-keyed map, not this in-place tweak.
See `docs/benchmarks.md` "M19 Tier-2".

**Denser int-keyed map/set index landed (2026-06-13):** the map index was
`FxHashMap<u64, Vec<usize>>`, paying a tiny `Vec<usize>` heap alloc per distinct key (200k of them in
`benches/chz/map.chz`) + a pointer-chase per lookup — yet numeric keys hash injectively, so every
candidate list is length 1. Collapsed the per-key `Vec` to an inline single position via
`enum Pos { One(usize), Many(Box<Vec<usize>>) }`, extracting the (formerly duplicated) `MapData`/`SetData`
index logic into one shared `HashIndex(FxHashMap<u64, Pos>)` in `src/vm/heap.rs`. `One` is zero-alloc/inline;
`Many` (real hash collisions only) is `Box`ed to keep `Pos` 2 words so struct sizes are unchanged.
`candidates`/`push` signatures are identical → **VM hot paths in `mod.rs` unchanged, parity by construction**
(interp keeps its `Vec<usize>` oracle; both confirm hits with `values_equal`). **`map` 2.68× → 1.94×
CPython (−26%, remeasured on merged HEAD `2a934a8`; the dev-base figure was ~1.7×/−36% — variance +
heavier base, see `docs/benchmarks.md` merge-remeasure note)** — the predicted target landed. Others flat (touch no
map/set). 2 new collision-upgrade guards (RED on a `One`-only stub, GREEN with `Many`), 1712 green,
conformance green, clippy clean. **Next `map` suspect:** `values_equal` per-probe cost + `FxHashMap`
lookup/rehash (no longer the `Vec` alloc). See `docs/benchmarks.md` "M19 — denser int-keyed map/set".

**Positional struct layout landed (memory-layout lever #1, 2026-06-16):** `Obj::Struct` instance
fields went from `Vec<(Box<str>, Value)>` to a flat positional `Vec<Value>` (hidden-class / `__slots__`
layout, `src/vm/heap.rs`). Field names now live only in `StructDef`; the runtime resolves them on the
**cold path** (Display/stringify/probe-miss/wire/snap) via `name`→`StructDef`, while the hot field
read/write (IC-guarded on `tid`) is a pure `fields[idx]`. This kills the **N per-field `Box<str>`
allocations per struct instantiation** + the per-field name-clone on `==` (now a by-position value
compare). The synthetic native structs `Match`/`Response` are registered in `Program.structs`
(`src/compiler/mod.rs`) so the runtime can recover their declaration-order names. The interp (frozen
oracle) keeps `Vec<(String, Value)>` per instance — **untouched**; both engines iterate fields in
declaration order, so Display/`==`/interpolation stay byte-identical (two-engine parity by
construction). **Bench-neutral** (the suite is dispatch/alloc-bound and the `struct` bench reuses
instances — predicted in `gaps.md`), but a 4-field struct-construction micro went **827 ms → 510 ms
(−38%)**; primary value is the alloc reduction + **JIT groundwork** (positional storage → constant
field offsets Cranelift codegen needs). 1968 green (+2: positional-layout type guard +
`struct_layout.chz` two-engine golden), conformance 7/7, clippy clean. See `docs/benchmarks.md` "M19
memory-layout lever #1" + `docs/future.md §4`. **Land order #1 ✅ → #3 (closure captures) ✅ → #2 (enum
variant id) ✅ — sequence complete.**

**▶ Next perf batch (Tier 1 DONE — Phases 6+7 landed, 8 deferred; Tier 2 is next; full detail +
`file:line`s in [`docs/future.md §4` "Post-M19 next levers"](docs/future.md)).** Diagnosis: the
remaining gap is **call frame-setup + the alloc/hash paths**, not per-op dispatch (Phase 7 took `loop`
to ~1.1×). Target is CPython 3.14 (specializing interpreter + optional JIT).
- **Tier 1 (cheap→medium):** ✅ 1. method-call IC + flatten `do_method_call` (Phase 6, `struct` −9%).
  ✅ 2. trim per-op overhead in `run_until` — landed as **inline hot ops** (Phase 7; every op-bound bench
  faster, `loop`/`list` −15/−17%). The other two sub-levers (lazy `span`, serial/MN loop split) were left
  unshipped — predictably-false cheap branches, low expected payoff vs the inline win; revisit only if a
  profile shows them. ⏸️ 3. call-site specialization for `Op::Call` — **deferred (no-gain after inline);**
  see the Phase 8 bullet above + `docs/benchmarks.md`.
- **Tier 2 (structural):** ✅ 4. **adaptive opcode quickening (PEP 659) — v1 binops LANDED (2026-06-13):**
  the un-fused generic binop arms (`Add..GtEq` reached by stack operands; `Eq`/`NotEq`, never fused)
  specialize to an int/int fast path behind a per-`Vm`, per-site `(proto,ip)` deopt guard. Side table
  (`quicken: Vec<u8>` + `quicken_base` prefix-sum) mirrors `field_ic`/`method_ic` — heap-independent, not
  swapped, **no `Op`/compiler/interpreter change → parity by construction**. Measured: **`primes` −7–8%**
  (its never-fused `% … == 0` int `Eq` left `values_equal_guarded`), `fib` marginal, others flat (fused /
  alloc / hash-bound — as scoped). Gotcha pinned by test: the int `Eq` fast path **replicates the generic
  lossy `as_f64==as_f64`** (so `2^53 == 2^53+1` stays true), not exact `x==y`, to keep parity. 6 new guards,
  1613 green, clippy clean. See `docs/benchmarks.md` "M19 Tier-2 … quickening, v1". ✅ **CallMethod
  adaptive LANDED (2026-06-13): `poly_method` −33% (6.0× → 4.28× CPython)** — the method-call IC's
  single `MethodIcCell` is widened to an N-way (4-way) `MethodIcSite` with the binop quickening's
  one-way sticky-deopt: a bounded-megamorphic site (≤4 receiver types) HITS a way per type and flattens
  instead of refill-thrashing through a per-miss `StructDef` clone; a 5th distinct type latches `sticky`
  and goes slow (clone-free: borrows `Arc<Program>.structs` instead of cloning the `StructDef`). Side
  table still int-only (tids/proto/module-idx), no `GcRef` — heap-independent, parity by construction
  (interp has no IC). New `poly_method` bench + 5 guards + golden `examples/poly_method.chz`; 1838 green.
  This *unifies* the field/method caches under one adaptive form (`GetIndex`/`SetIndex` already got their
  Int-key fast path in #5 below, so they are covered). ✅ 5. **map/list index specialization** (`mod.rs`
  `GetIndex`/`SetIndex`) — **landed (Int-key fast path + inline dispatch): `list` −4%, `map` neutral**
  (hash-probe-bound). The remaining `map` win shipped as its own lever — ✅ **denser int-keyed map/set
  index LANDED (2026-06-13): `map` 2.68× → 1.94× CPython (−26% on merged HEAD)** — `Vec<usize>` candidate list → inline
  `Pos::One` / `Pos::Many` overflow in a shared `HashIndex` (`src/vm/heap.rs`). See the landed note above.
- **Tier 3 (big, separate):** 6. **Cranelift method-JIT** (end-game; the only path to match/beat fib;
  #4 is the stepping stone). 7. NaN-boxing (BLOCKED, above). 8. register VM / generational GC (low ROI).

### Robustness pass (landed, both engines)
- **Map/Set now snapshot a struct/enum/newtype key on INSERT (Go value-key model, 2026-07-12).**
  A `struct`/`enum`/`newtype` key/element was stored BY REFERENCE (aliased to the caller's live
  value). Because structs are mutable, mutating the value AFTER using it as a key/element silently
  corrupted the collection (`m[a]="x"; a.x=2; m[a]` → `key not found` on the object you hold; Set
  dedup/algebra broke without a fault). Fix: `snapshot_key` (`vm/arith.rs`, beside `hash_key_rooted`)
  deep-copies the key **only** for the three heap-aggregate arms `hash_value` dispatches
  (`Obj::Struct|Enum|NewType`); scalars / immutable `Str`/`Bytes` pass through unchanged (zero-clone
  hot path — no M19 scalar-key regression). It does **not** reuse the airlock `deep_clone`
  (`to_wire`/`from_wire`): that serializer FAULTS on a generator / captured `ref` / cyclic key (all
  legal, previously-working sequential keys) and re-stamps by-reference sub-values (a closure /
  `Channel` / `Shared` field) with FRESH handles that `values_equal` — identity-only for those arms —
  then never matches (a stored-snapshot ≠ live-key lookup miss). Instead a dedicated `snapshot_value`
  copies only the mutable, structurally-`==` arms (`Struct/Enum/NewType/List/Tuple/Map/Set/ByteArray`)
  and keeps every identity/by-reference sub-value by handle, so the snapshot stays `values_equal` to
  the original; it is pure-alloc (no VM re-entry ⇒ no GC mid-copy ⇒ no rooting), visited-map-deduped
  (no DAG blow-up) and `MAX_STRUCTURAL_DEPTH`-capped. Wired at every insert site: map index-set,
  `set.add`, `Op::NewMap`/`Op::NewSet` literals, `Set(iterable)`/`Map(iterable)`, and **`Map.update` /
  `Map.merge`** (the spec-listed paths the first cut left aliasing — `update`/`merge` now snapshot the
  incoming keys, and `merge` builds fresh instead of `clone()`-aliasing the receiver's keys).
  **Values are never copied** (mutating a stored value in place stays intended); the transient lookup
  key (`m[k]`, `k in m`, `s.has(k)`) is never snapshotted. **Ceilings** (both match pre-change
  behavior, no new fault): a **cyclic** struct/enum key — or one nested deeper than
  `MAX_STRUCTURAL_DEPTH` (10000) — is stored by reference (a value-copy of a cycle can't compare equal
  to the original; a too-deep snapshot would alias its tail then miss on lookup, and the pre-snapshot
  cycle/depth check `store_key_by_reference` is itself depth-capped so it can't overflow the host
  stack → SIGABRT), so mutate-after-store can still corrupt it; set-algebra
  RESULT aliasing (`union`/`intersection`/`difference`) is a distinct out-of-scope surface. Tests:
  `map_struct_key_snapshot_on_insert`, `set_element_snapshot_algebra`, `all_insert_paths_snapshot`,
  `scalar_keys_unchanged`, `map_value_not_snapshotted`, `cyclic_struct_key_inserts_and_resolves`,
  `generator_field_key_inserts_and_resolves`, `closure_field_key_resolves_after_insert`,
  `map_update_merge_snapshot_keys`, `deep_acyclic_struct_key_inserts_and_resolves` (parity_tests),
  `map_struct_key_snapshot_survives_gc_stress` (gc_tests). Docs: `docs/syntax.md` Hashable section.
- **Import-alias guard now symmetric on reserved TYPE names (Finding B, 2026-07-10).** The
  `import X as ALIAS from M` / `import M as ALIAS` guards rejected reserved CALLABLE aliases
  (`import who as int` → `reserved (builtin)`) but silently ACCEPTED reserved TYPE names
  (`import who as Result from lib` was check-ok — likewise Option/Iterator/Ref/Socket/Listener/ptr/
  owned_str), asymmetric with the struct/enum/type DECL guard which already rejects all of these via
  `is_reserved_type`. Fix (checker-only name resolution, `src/checker/mod.rs` + two guard sites in
  `src/checker/setup.rs`): new free helper `is_reserved_alias_target = is_reserved_name(n) ||
  (is_reserved_type(n) && n != "nil")` reused at both alias sites — reuses the existing predicate (no
  second list). `nil` is carved out (it is a shadowable value-builtin: `nil := 5` is accepted, so
  `import x as nil` stays legal). Un-aliased/self-renamed importable reserved types
  (Socket/Shared/Executor…) still import via the preserved `a != member` clause; fresh non-reserved
  aliases (`import who as Helper`) still bind and are usable. No VM/runtime change; serial==M:N parity
  trivially preserved (rejected program never reaches either engine).
- **Generic static-factory un-inferred type-param soundness hole (2026-07-07).** A generic struct/enum
  STATIC (associated) method returning `Type[T]`, called with NO type-level turbofish AND no binding/return
  annotation, left the enclosing `T` as `Ty::Unknown` un-flagged — which then swallowed any later argument,
  defeating homogeneity (`b := Box.empty(); b.add("hello"); b.first() + 1` check-ok → runtime trap; and a
  silent int+str heterogeneous store). Fix (checker-only, `infer_static_call` in `src/checker/expr.rs`): (1)
  the un-inferred-param degrade loop now iterates only the method's OWN `[U]` params — the ENCLOSING type's
  params leak as `Ty::Param`, so the first mismatching/mutating use routes to the existing "un-inferred type
  parameter … bind it at the construction site" / "expected T, found <ty>" diagnostics (parity with the
  already-sound generic free-fn path); (2) a `seed_from_hint(expected, &sig.ret, …)` seeds a still-free
  enclosing param from a `let`/return annotation (`b: Box[int] = Box.empty()`), threaded through all 6 static
  call sites. STILL works: `Box[int].empty()` (turbofish), `b: Box[int] = Box.empty()` (annotation),
  `Box.of(5)` (arg-inferred). Method-own `[U]` unbindable from args stays refinably `Unknown`. Covers generic
  enums (`Wrap.none()`) via the shared path. Both engines + graph-path (`entry_rejects`) tested.
- **Bounded infinite-recursion stack trace (gap #8, 2026-06-23).** At `MAX_CALL_DEPTH` (10_000) a
  recursion fault used to print one `  at <fn> (called at …)` line per frame → ~10_001 lines flooding
  the terminal. `format_trace` (rendered byte-identically in `vm/mod.rs` + `interp/mod.rs`) now (1)
  collapses runs of consecutive same-name frames to the innermost `at` line + `  … (× N more identical
  frames) …`, and (2) caps the collapsed list to head `TRACE_HEAD=10` / tail `TRACE_TAIL=10` with a
  `  … (M frames elided) …` marker. A recursion fault now prints ~4 lines; the captured `Vec<TraceFrame>`
  is untouched (debuggers/tests still see every frame). No-op for small distinct-name traces, so the
  exact-trace golden (`examples/stack_trace.chz`) is unchanged. Parity-tested both engines.
- **Cyclic-data depth guard + order-independent map `==`.** Two fuzzing-found bugs: a cyclic struct made
  `print`/`==` recurse unbounded on the host stack (uncatchable SIGABRT, even inside `recover:`); and map
  `==` was order-dependent while set `==` was order-independent. Fix: `MAX_STRUCTURAL_DEPTH = 10_000`
  threaded through display + a `values_equal_guarded` (the public `values_equal -> bool` stays a thin
  wrapper, so the ~66 hash-probe call sites are untouched); the recoverable depth-exceeded error surfaces
  only at the `==`/`!=` op sites. Map `==` is now order-independent value equality. (Interp's *call*-depth
  overflow in **debug** builds is left as-is — the tree-walk engine is slated for removal; release + VM
  are fine.)
  - **Airlock cyclic-sendable guard (2026-07-04).** The same class of bug survived on the concurrency
    airlock: copying a value across a task boundary (`spawn` arg / `Channel.send` / `Shared(...)` /
    worker return / M:N module-global snapshot) deep-walks it via `Vm::to_wire` / `to_snap` (src/vm/sched.rs)
    with **no depth guard**, so a check-accepted cyclic sendable overflowed the host stack → uncatchable
    SIGABRT on **both** engines. Fix: extend the same `MAX_STRUCTURAL_DEPTH = 10_000` recoverable-error
    guard into a shared depth-counted worker behind both serializers (`to_wire`→`to_wire_depth`,
    `to_snap`→`to_snap_depth`, fast path threads the shared budget) so serial `to_snap` and M:N `to_wire`
    trip at the identical depth; a cyclic value now degrades to the catchable `maximum structural depth
    (10000) exceeded (cyclic data structure?)` error, byte-identical serial vs M:N. Two `to_wire`
    call-sites already holding a span (serial `Executor.submit`, `#[cfg(test)]` worker return) route
    through `to_wire_at` so the error reports the real site, not line 0. Wide-but-shallow sendables (100k
    elements) still cross fine (the counter measures nesting depth). golden `examples/airlock_cycle.chz`
    + 5 unit tests (both-engine spawn/channel/shared, M:N-only module-global, wide-acyclic-crosses-fine).
    **(SUPERSEDED 2026-07-21, gaps.md item A:** self-referential data now ROUND-TRIPS via identity-
    preserving container serialization — the depth cap stays only for genuinely-unbounded ACYCLIC nesting,
    and `airlock_cycle.chz` FLIPPED to crossing. See the item-A entry near the top of this file.)
- **`defer:` block form** — `defer` takes an indented block as well as a single call (multi-action cleanup
  without N `defer` lines), mirroring `spawn`'s dual form with no new VM op. Body runs top-to-bottom at
  scope exit, LIFO as a unit, free vars snapshot by value at the `defer` point, runs on all exit paths.
  A dedicated `defer_floors` write-gate rejects reassigning an enclosing local inside the block (no
  `SetCaptured` op); a `?` short-circuit inside the block is absorbed on both engines.
- **Integer `List.sum()` checked-add (2026-06-25).** The integer accumulation in `List.sum()` used a raw
  `acc += *n` on both engines — `[i64::MAX, 1].sum()` silently wrapped to `i64::MIN` (release) / host-
  panicked (debug) instead of faulting, while every other integer add (`+`, `+=`, `fold`, `*`, `/`) is
  checked. Now `acc.checked_add(*n)` raises the same recoverable `integer overflow in Add` at the
  `.sum()` call-site span, byte-identical to `+` (VM `vm/mod.rs` + interp `interp/builtins.rs`). The
  any-float path is untouched (accumulates to `float`, may reach `inf`). `examples/overflow.chz` now
  exercises the `sum` case alongside `math.abs`; two-engine parity tests `parity_list_sum_overflow` /
  `parity_list_sum_mixed_float`.

---

## Concurrency — feature-complete (confirmed 2026-06-12)

Core implemented through **M21** (still evolving; M19 perf in progress); **concurrency shipped through Tier-D (D0–D6c) + M-C**. The surface —
`spawn` / `parallel:` nursery / `Channel[T]` / `Shared[T]` / `Executor`, plus the VM's real OS-thread
engine and the netpoller + `std.net` — is complete and stable. **M-C implicit nurseries shipped
(2026-06-12)** — every function body and the module top level is an implicit nursery; a bare `spawn` is
legal anywhere and joins at `return`/end. ~1592 tests green; the cooperative engine (`--serial`) and the
OS-thread engine stay byte-identical on every `examples/parallel*.chz` + `examples/implicit_nursery.chz`
golden, and the frozen interp is the differential parity oracle for the sequential subset.

**CLI engine selection.** `chezzi run` now defaults to the OS-thread engine; `--serial` selects the
cooperative single-thread VM (the frozen parity oracle), `--parallel` is an accepted no-op alias, and
`--threads=N` (or env `CHEZZI_THREADS`, flag wins; `0`/omitted = all cores) sizes the OS-thread worker
pool via `vm::worker_count()`. `--threads` errors with `--serial`/`--interp` (neither is multi-threaded).

**`std.cancel` — cancellation tokens + `Channel.trip()` SHIPPED (2026-06-15).** A user-level
cooperative cancellation **`Token`** (Go-`context`-inspired, adapted): `cancel.manual()` /
`cancel.timeout(ms)`; methods `cancelled()`, `reason()` (`"cancelled"`/`"timeout"`), `done() ->
Channel[bool]` (a `wait:` arm), `cancel()` (anytime/any task), `deadline_at()`. **Tree propagation
landed** (see the next note). Pure Chezzi
(`std/cancel.chz`) over `Shared[bool]` +
`monotonic()` (deadline checked **at poll time** → timeout is deterministic across engines, no
background canceller) + ONE new native primitive **`Channel.trip()`** — a permanent level-trigger
latch (the manual-cancel fan-out a move-on-send `Channel` lacks; reuses `close()`'s wake fan-out
minus `closed`). Decoupled from the internal nursery cancel flag (so a user `cancelled()`-return runs
`defer`/`recover:` normally). Goldens: `examples/channel_trip.chz`, `cancel_manual.chz`,
`cancel_timeout_wait.chz` (byte-identical on cooperative-VM + interp); `examples/cancel_cpu.chz`
carries **no `.expected`** (manual cancel of a CPU sibling diverges by engine — default preempts,
`--serial`/`--interp` run to completion) and is covered by a Rust `#[test]`. A cross-task
cancel→`wait:` lost-wakeup regression (`MnSched::park`/`park_wait` gap re-check now includes
`done_latch`) is guarded by `cancel_trip_wakes_parked_wait_under_parallel`. Closes the `gaps.md`
cancellation gap (timeouts + manual cancel). See `docs/concurrency.md` §6e/§6c'.

**`std.cancel` TREE PROPAGATION — parent/child derivation SHIPPED (2026-06-17).** `Token.derive()`
(and the free-fn `cancel.derive(parent)`) builds a **child** token (Go `context.WithCancel`):
cancelling or timing-out a parent cancels every transitively-derived child, recursively root-to-leaves,
while cancelling a child **never** touches the parent (one-directional). The link is **live** — a
parent flip is observed by an already-derived child, *including one that crossed the
`spawn`/`parallel:`/`Channel` airlock* — because the link is the parent's `Shared` flag plus a `Shared`
registry of descendant `done()` channels, which cross as live cores exactly like the flat token's `flag`
(so the feature is automatically three-engine consistent — **zero Rust changes**, no checker change:
`sendable_rec` already permits the self-referential `parent: Token?` field + `Shared`/`Channel`/`Option`
arms). A child inherits the **tightest** deadline (soonest absolute of itself + ancestors; an
already-elapsed-timeout parent yields a child cancelled at once with reason `"timeout"`, its `done()`
ready via its own timer armed to 0 ms). `done()` cascades **transitively**: `derive()` registers a
child's `done()` channel into **every ancestor's** registry (walking the parent chain to the root, each
insert an atomic `Shared.update()` so concurrent siblings don't lose updates), so a manual `cancel()` at
ANY depth above trips the descendant's `done()` directly — a grandchild parked in `wait: leaf.done()`
wakes on a grandparent cancel, not just on its immediate parent. `reason()` is nearest-cause-wins
(self's own cause, else inherited). Goldens: `examples/cancel_tree.chz` + `.expected` (byte-identical on
`run`/`--serial`/`--interp`; `golden_cancel_tree_via_run_file` VM + `golden_cancel_tree_chz` interp
twin), plus eight VM unit tests (`cancel_child_*`, `cancel_transitive_grandchild`,
`cancel_grandchild_done_ready_after_grandparent_cancel` + `cancel_great_grandchild_done_ready_after_root_cancel`
— the transitive-`done()` guards, `cancel_token_sendable_with_parent` — the cross-airlock live-link
guard). **Known v1 limit:** the per-ancestor registry only **grows** (no token-drop hook); tokens are
request-scoped/short-lived, a future prune-on-cancel could clear it. Closes the `gaps.md`
tree-propagation gap. See `docs/concurrency.md` §6e.

> **`Channel.recv_timeout(ms)` — attempted then reverted (2026-06-12).** A bounded-wait `recv` was
> implemented with a **demote-always** shortcut (reuse `demote_recv_block` + a deadline) to avoid the
> heavier park+timer machinery. The review panel found it **unsound at `native_reentry == 0`**: (1) a
> top-level M:N `recv_timeout` demotes the worker, and a later reduction-budget yield strands the fiber →
> **silent hang**; (2) the cooperative park path reused `park_recv` (built for 0-arg `recv`) but
> `recv_timeout` has `argc=1` → **stack corruption** on resume; (3) cooperative-nursery no-producer faults
> `deadlock` not `None`, and demote-failure faults (not total). Reverted (commit `653dfd2`). **Lesson: the
> correct design is the heavier one** — at `native_reentry == 0`, snapshot-park on a timer (claim-flag +
> a `MnSched::timeout_wake` racing `send_wake`, like the socket-timeout `poll_timed_out` path), demote
> only at `native_reentry > 0`; cooperative needs a recv_timeout-aware quiesce (resolve-to-`None`, not
> fault) or accept the documented deadlock-fault divergence. Checker `Ty::Int → Option[elem]` sig + interp
> poll-once arm were correct; the VM scheduler integration is the hard part. A proper follow-up, not a
> drop-in. (`Atomic[T]` + `timer(ms)` have since **shipped** — see `concurrency.md` §6b/§6c,
> `examples/atomic.chz`. `wait` — Chezzi's `select` — is **designed + locked** (`concurrency.md` §6d),
> not deferred for lack of a design; it just awaits implementation as its own focused milestone.)

> **Concurrency follow-ups — `Atomic[T]` + `timer(ms)` LANDED, `recv_timeout` DROPPED, `wait` designed
> (2026-06-13).** Brainstormed the deferred trio and shipped two of three; `recv_timeout` is dropped as
> redundant.
> - **`Atomic[T]`** (commit `07ae080`) — generic atomic box mirroring `Shared[T]` (Mutex-backed, sendable
>   handle, value-first `Atomic(v)`): `load`/`store`/`exchange`/`cas` for any `T`, `add`/`sub` on numeric
>   `T` (checked-overflow like `+`/`-`). Two-engine parity; `--parallel` add/cas atomicity stress tests
>   (300-thread exact sum, 200-fiber CAS-retry). See `docs/concurrency.md §6b`.
> - **`timer(ms) -> Channel[bool]`** (commit `cd1673e`) — one-shot, **level-triggered** timeout channel.
>   Delivery is scheduled **at `recv` time in the receiver's own scheduler** (NOT at construction — a
>   top-level timer can be recv'd in a `--parallel` child): `--parallel` schedules a background `send` +
>   parks (accounted `inflight` so no false deadlock); cooperative VM / interp / callbacks inline-sleep to
>   the deadline (like their `sleep_ms`). 3-engine parity. Adversarial review (Reality Checker + Code
>   Reviewer) found **no Critical/Important** — sound park-gap (reuses `MnSched::park`'s queue re-check),
>   no inflight leak (job holds Arcs + always `fetch_sub`s), no double-schedule (queue-first on re-run).
>   Known v1 limitation: `timer.recv()` inside a native callback pins a worker (no demote). `docs §6c`.
> - **`recv_timeout` DROPPED** — `wait` + `timer` subsume it (`ch.recv_timeout(500)` ≡ `wait` over `ch`
>   and `timer(500)`), and it was the unsound/reverted one. No separate primitive.
> - **`wait` (select) — SHIPPED on ALL THREE engines (2026-06-13; M:N blocking park landed 2026-06-13).**
>   Full design + grammar + per-engine semantics in **`docs/concurrency.md §6d`** (cheat row in
>   `docs/syntax.md §11b`; `examples/wait_select.chz`). A `wait:` compound statement races channel
>   `recv`s — arms `v := ch.recv():` (`:=`/`=`/`_` targets), optional non-blocking `else:` (last), `timer`
>   arms, recv-only (unbounded channels → sends never block); source-order priority; closed+empty arm
>   **skipped**; all-closed+no-`else` faults. **Done:** lexer→parser (`parse_wait`)→checker (`check_wait`)
>   →interp (`exec_wait`, the parity oracle)→cooperative VM (`Op::WaitPoll` + `compile_wait`), incl. the
>   **cooperative multi-channel park** (one fiber filed under N keys via `wait_suspend`/`run_child`, swept
>   out of the other buckets on resume — `vm_wait_blocks_then_wakes_on_second_channel` +
>   `vm_wait_sweeps_other_buckets_after_waking`). **M:N `--parallel` blocking park — LANDED:** a blocking
>   `wait` now parks under `--parallel` instead of faulting. ONE `WaitPark { fiber, keys, claimed }` held
>   behind an `Arc`, with a `ParkedEntry::Wait(token)` filed in every arm's `MnSched.parked[key]` bucket
>   (`MnSched::park_wait`, the N-key generalization of `park`); the first waker CASes `claimed`, takes the
>   fiber, and sweeps the stale token out of all other buckets under one core-lock hold
>   (`send_wake`/`close_wake`/`cancel_drain`/`flag_deadlock` all token-aware). Routed via
>   `Disp::WaitPark(Vec<(key, core)>)` captured while the fiber heap is live (mirrors `Disp::Park`). The
>   1-key recv park stays the cheaper `ParkedEntry::Recv` case (alloc-free, byte-identical —
>   `vm_wait_single_arm_recv_park_unchanged_under_parallel`). Deadlock accounting: a wait-parked fiber is
>   `parked_n += 1` (ONE fiber, regardless of arm count) so the `is_deadlocked` predicate stays sound
>   (`vm_wait_lone_blocked_parallel_deadlocks`; a live sibling vetoes —
>   `vm_wait_sibling_send_vetoes_deadlock_parallel`). **`native_reentry > 0` (wait inside a native
>   callback):** can't snapshot-park → `demote_wait_block` blocks in place, polling all N arm queues
>   source-order on a bounded `DEMOTE_POLL_BACKOFF` (the N-arm analogue of `demote_recv_block`;
>   lower-throughput-but-sound **v1 limitation** — there are N channel condvars, no single one to block on).
>   All three engines byte-identical on `examples/wait_select.chz`; 150× + 4×80× stress loops clean (no
>   lost-wakeup). **Fixed in passing (a pre-existing two-engine parity bug exposed by the edge tests):**
>   the peephole optimizer did not relocate `Op::WaitPoll`'s `arm_targets`/`else_target` through its
>   fold/fuse index remap, so a multi-arm `wait` whose arm body fused a binop (`x + w`) jumped PAST the
>   bind prologue (VM 65 vs interp 66). Now `WaitPoll`'s targets are marked + relocated like `Jump`/
>   `MatchArm` (`relocates_waitpoll_arm_and_else_targets_past_a_fold`,
>   `vm_wait_arm_body_outer_local_in_binop_matches_interp`).

### Tier-D — complete (D0–D6c)

Designed in [`docs/concurrency.md §10`](docs/concurrency.md); the full per-phase TDD breakdown lives in
**[`docs/concurrency-tier-d.md`](docs/concurrency-tier-d.md)**. Landed, in one summary:

- **D0** — O(N²)→O(N·logN) cooperative ready-queue (per-nursery `ready` set + parked-index buckets).
- **D1** — lazy module snapshot: a shared read-only `Arc<ModuleSnapshot>` faulted into each worker heap
  on first access, killing the per-task module-graph rebuild.
- **D2a/D2b** — true **M:N work-stealing scheduler**: lightweight share-nothing fibers (own heap, carried
  in a swappable `FiberCtx`) multiplexed over the bounded pool, **parking on `recv` instead of pinning OS
  threads**; the joining thread runs an inline shell that alone guarantees completion (decision B).
- **D3** — BEAM-style **reduction-counting preemption** (`reds` budget, yield at exhaustion to the run
  queue's tail) so a CPU-bound fiber can't starve siblings.
- **D4** — Go-style per-worker local run queues + shared global overflow + random-victim work-stealing +
  periodic global check; runnable-gated park wake (a true `cv.wait` when `runnable==0`, bounded backoff +
  re-steal when `>0` — the mutex *is* the StoreLoad barrier, no Go-style fence needed).
- **D5** — **dirty/blocking pool**: a blocking off-heap-safe native (`read_file`/`write_file`, `fs.*`,
  `request`, `process`, `sleep_ms`) suspends the fiber and hands the call to a growable pool instead of
  pinning a core worker; an `inflight` fiber-state vetoes a false deadlock. A process-wide timer thread
  (later folded into the poll thread) parks sleepers on a deadline min-heap. *Path C* demotes the worker
  (one raw replacement OS thread, Go-`handoffp`-style) for a blocking `recv`/`sleep`/socket op reached
  *inside a native callback* (`native_reentry > 0`, host-stack loop frame, unsnapshotable).
- **D6a/b** — **netpoller** (`src/vm/poller.rs`, epoll/kqueue via `polling`): a would-block socket op
  becomes a cheap fiber-park. `std.net` (`Obj::Socket`/`Obj::Listener` over `Arc` cores) — non-blocking
  `connect`/`listen`/`accept`/`read`/`write`/`close`/`addr`; `connect` is true non-blocking via
  `socket2`. Drain-on-fault re-injects socket-parked fibers so a net server can share a nursery with a
  fallible sibling; one poll thread serves both socket readiness and sleeps.
- **D6c** — **per-socket read/accept/write timeout** (`--parallel`): `conn.read(n, timeout_ms)` /
  `sock.write(s, timeout_ms)` / `server.accept(timeout_ms)` return `Err("timeout")`; `0` polls once, a
  negative saturates. Reuses D6b's deadline-bounded poll, no new thread/heap/job (`poller::Parked` gains
  a `deadline`, a `fire_due_socket_timeouts` pass sets a per-fiber `poll_timed_out` marker). Checker
  gained optional trailing-arg arity. `examples/socket_timeout.chz`.

**Per-connection `spawn`** also landed — an **eager injectable nursery** (`--parallel` M:N, ≥2 cores): a
`spawn` in a *nested* `parallel:` runs concurrently with the rest of the body instead of queueing for the
join, so the canonical server shape (accept-loop `spawn`s a `handle(conn)` per connection) works. The
nested nursery is eager (`EnterNursery` builds the `MnSched` immediately + spawns one dedicated raw
drainer thread); a `spawn` injects a live fiber straight into it; a `body_open` flag holds termination
open and vetoes the deadlock predicate while the body may still inject. **v1 limits (documented):** needs
≥2 hw threads; bounded accept loops only (an unbounded `while true:` server never reaches the join —
graceful shutdown is future work); a handler talking back to the acceptor via a Channel is a cross-nursery
wakeup. `examples/echo_server_spawn.chz`.

**Cross-nursery flat scheduler — M:N (`--parallel`) DONE, cooperative DEFERRED.** The circular
outer-sibling cross-nursery deadlock (`examples/parallel_cross_nursery_circular.chz`: `inner()` spawns a
nested nursery while `main`'s outer `parallel:` still has an un-run sibling `O`; the inner owner used to
drain only its private queue and could never RUN `O` → `deadlock` fault) is **fixed under `--parallel`**:
- **One VM-global `MnSched`** with `SchedCore.scopes: Vec<JoinScope>` (replacing the scalar
  `{done,total,body_open}`) + a flat `slots` vec. Each nested nursery is a SCOPE enlisted into the SAME
  global run queue; `Fiber` carries a `scope_id`. The inline owner returns on a **scope-scoped stop**
  (`Take::Stop` when ITS scope's `done==total`, having drained the GLOBAL queue meanwhile — so it ran the
  cross-nursery sibling), while farmed helpers drain until global `terminate` (a `SENTINEL_SCOPE` owner id).
- A nested builder **early-enlists** the outer nursery's still-pending siblings (so the nested owner can
  run them — the cross-nursery wake) but **DEFERS** each enlisted scope's output flush to its OWN
  `JoinNursery` (`mn_scopes` records the scope; `mn_enlist_sched` holds the sched alive until the last
  enlisted scope joins). This preserves the **per-nursery-join flush order**, so three-engine parity for
  non-blocking nested spawns is byte-identical (`implicit_nursery_nested_functions` etc. unchanged).
  Outer scopes are enlisted **before** any helper worker is farmed, so a multi-task inner nursery can't
  trip the global deadlock predicate before the outer sibling is seeded (caught + regression-guarded by
  `examples/parallel_cross_nursery_fanout.chz` — a 2-task inner nursery, looped under a watchdog).
- The deadlock predicate + `finish`/`flag_deadlock`/`cancel_drain` went **global over scopes** (fault only
  when SOME scope is incomplete and nothing can progress anywhere); per-scope **cancel** Arcs (the shell's
  `self.cancel` re-pointed to the running fiber's scope cancel on each `run_one_fiber` swap-in;
  `cancel_drain(scope_id)` requeues only that scope's parked fibers) keep an inner fault from cancelling
  outer siblings (structured concurrency preserved). Genuine no-sender deadlocks still fault
  (`golden_parallel_deadlock_still_faults`, 30s watchdog).
- **Output order note:** because `O` (outer) and `I` (inner) live in DIFFERENT nurseries with different
  join points, the M:N flush order is `I` (inner join) then `O` (outer join) — i.e.
  `I got 1\nO got 1\ndone` — NOT the case-C single-nursery order (`O got 1\nI got 1`). Both complete; the
  ordering follows the parity-preserving per-nursery flush.
- **Eager nurseries keep their private sched (OPTION A), + child→parent wake routing (gaps.md B5):** the
  per-connection eager nursery keeps its OWN sched + dedicated drainer (single-scope fast path). But a
  `send`/`close` inside an eager body only scanned that private sched's park set, so a receiver parked in
  the PARENT nursery on a shared channel was never woken → a spurious `deadlock` (an uncontended nested
  `send`→outer `recv`). Fixed by `MnSched::parent_wake` (the eager sched points at the activating parent
  sched; `send_wake`/`close_wake` walk that chain child→parent, strictly upward, and requeue the parent's
  parked receiver onto its home sched). `is_deadlocked` unchanged — genuine no-sender quiesce still faults.
  Golden `parallel_cross_nursery_nested_send_to_outer_recv.chz` (serial==M:N). **Residual (documented):**
  parent→child (receiver parked INSIDE an eager body, sender in an ancestor) + sibling-eager→sibling-eager
  stay timing-divergent (complete or deadlock-fault cleanly).
- **Cooperative (`run --serial`) + `--interp`:** still serialize nested nursery levels → the same program
  **still faults `deadlock`** there. The cooperative-engine flatten is a **separate, later commit**.
  Workaround on `run`: siblings in ONE nursery (doc case C). Golden is M:N-only (no coop/interp leg),
  watchdog-wrapped — mirrors `golden_channel_block`.
- **Post-review hardening (the first cut was REJECTED by the adversarial panel — 3 blocking; now fixed):**
  - **Inline outer-body `send`/`close` routing (charges #1/#2):** the inline `parallel:` builder runs with
    `self.mn == None` (sched only in `mn_enlist_sched`), so its own `send`/`close` used to bypass the
    global park set and never wake an enlisted, parked sibling → false `deadlock`. `channel_send_wire` +
    the `close` arm now route through `self.mn.or(self.mn_enlist_sched)`. Guards:
    `..._inline_send.chz`, `..._inline_close.chz`.
  - **`awaiting_builder` deadlock veto:** an early-enlisted scope is marked `awaiting_builder` (the live
    builder body is its feeder); `is_deadlocked` vetoes only while EVERY incomplete scope is awaiting the
    builder (`all_incomplete_awaiting_builder`). A genuine NESTED deadlock keeps a non-awaiting scope
    incomplete → still faults (`parallel_cross_nursery_genuine_nested_deadlock_still_faults`).
  - **Late spawn after enlist (charge #3):** a `spawn:` issued after `early_enlist_outer` drained the
    nursery vec used to be silently dropped at the join. `join_nursery` now runs the refilled tasks on
    the HELD flat sched (`mn_enlist_sched`) as a fresh trailing scope — `register_scope` is append-only
    (slots stay contiguous) and un-latches a stale global `terminate` so the inline owner runs the late
    task instead of stopping on the prior-scopes-all-done flag (no clobber of the held sched, no `index
    out of bounds` panic, no drop); `drain_escaped_nursery` reports them on an escape. Guards:
    `..._late_spawn.chz`, `parallel_cross_nursery_late_spawn_into_middle_runs`,
    `parallel_cross_nursery_late_spawn_escape_reports_pending`.
  - **Atomic enlist (charge #4):** `early_enlist_outer` now validates (prepares workers from clones)
    BEFORE consuming the nursery / registering a scope, so a `prepare_worker` `Err` (checker-gated
    backstop) can't leave an unseeded scope (hang) or a half-state — it unwinds cleanly.
  - **2+ enlisting levels — limit LIFTED (independent/normal nesting now RUNS):** the old blanket gate in
    `early_enlist_outer` ("2+ enlisting levels … aren't supported") was TOO BROAD — it regressed ordinary
    multi-level nesting (independent nested `parallel:` blocks with sibling/late `spawn:`s) that has no
    shared channel and never parks. The gate is GONE. Any depth of nested `parallel:` now matches the
    cooperative engine under `--parallel`. Only the genuinely-CONTENDED case (2+ live receivers racing ONE
    channel across nested scopes) remains divergent — and it is NOT gated: concurrent-divergent BY DESIGN
    (delivery order may differ, or it deadlock-faults; suspendable concurrency is VM-only/divergent), it
    only must never PANIC and never HANG. Guards: `parallel_cross_nursery_independent_3level_runs_all`,
    `parallel_cross_nursery_late_spawn_into_middle_runs`, `parallel_cross_nursery_contended_never_panics`,
    golden `examples/parallel_cross_nursery_multilevel.chz`.
    A late `spawn:` into a middle nursery runs on the HELD flat sched as a fresh trailing scope via
    `register_scope_seeded` — register + seed atomically under one core lock (mirrors `inject`), closing a
    `runnable==0` TOCTOU window where a SENTINEL helper could have falsely deadlock-faulted a parked outer
    receiver. Guard: `parallel_cross_nursery_late_spawn_parked_matches_coop`.
  - **Out of scope (documented separate limits):** the inline-body *blocking* recv (case B — wake-side
    fix only) and eager (per-connection) nurseries' private sched.

**`Channel.close()` + closed-channel semantics + `try_send` + `for v in ch:`** landed (both engines) —
the headline consumer-side feature giving clean producer→consumer termination (was: a consumer looping
`recv` after the producer was done could only deadlock-fault):
- `for v in ch:` — blocking iteration, drains buffered + future values, ends cleanly once
  closed-and-drained (Go's `for v := range ch`).
- `ch.close()` — idempotent, no args, wakes every parked/demoted receiver.
- `send` after close → faults; `recv` on closed-and-empty → faults (drains buffered first).
- `ch.try_send(v) -> bool` — the safe partner of `send` (`false` = closed; channels are unbounded, so
  closed is `send`'s only failure mode). `try_recv` unchanged (`None` on closed).
- Comprehension-over-channel (`[v for v in ch]`) is **rejected by the checker** (it would diverge — VM
  drains, interp oracle can't).

**Pending-`spawn`-drop on early `parallel:` escape → cancel-and-report** landed (both engines): a
`parallel:` body escaping via `?`/`return`/`break`/`continue` before the join now **cancels** unstarted
tasks (the same end-state a started sibling reaches under cancellation) and emits one byte-identical
stdout report line. VM routes a `drain_escaped_nursery` through four reclaim sites (`do_return`, the
recover-catch fault path, a net-new `Op::ReclaimNursery` for break/continue, and the `do_try` recover-
scoped-`?` short-circuit, which drains the escaped body's defers to its floor *before* the report so
interp order is restored).

### Group B (B3.0–B3.6) — the OS-thread multicore epic, complete

Decomposed and documented in **[`docs/concurrency-b3.md`](docs/concurrency-b3.md)** (validated
shared-nothing architecture, decisions A–G, risk register). Summary of the landing:

- **B3.0–B3.2** — a `WireValue` airlock (`src/vm/wire.rs`) replaced `deep_clone`; `Channel`/`Shared`/
  `Executor` cores moved out of the GC heap into `Arc<…Core>` (`src/vm/core.rs`); `program` went
  `Rc<Program>` → `Arc<Program>`; `Vm::spawn_worker`/`run_task_isolated` build an isolated worker `Vm`
  with its own heap and cross args/captures/result by wire (cross-heap safety enforced via
  `ensure_crossable`). All single-thread, behavior byte-identical.
- **B3.3** — `str` crosses by value (`WireValue::Str`); the **G1 module-globals checker gate** (mutating
  a module global reachable from a `spawn` task is a type error, *"use Shared[T]"* — scope-aware,
  transitive over the free-fn call graph); worker module-graph reconstruction (read-only `home` snapshot
  + method tasks); then **real OS threads behind `--parallel`** (bounded pool, parent participates inline,
  per-core condvar `recv`, `Shared.update` lock).
- **B3.4** — cooperative **cancellation** + cross-thread `os.exit` (per-nursery `cancel` flag, first
  fault/exit trips it; `os.exit` wins over any sibling fault; cancel bypasses `recover:` but still runs
  `defer`s). Single-level only — nested-nursery cancel propagation is documented/deferred.
- **B3.5** — nursery-local **deadlock detection** under threads (barrier-confirm detector; later retired
  in favour of D2b's exact single-coordinator predicate).
- **B3.6** — `Executor` on the pool + the **A3b `submit`-capture sendability gate** (checker). A
  submitted closure crosses **by value on BOTH engines** (`WireValue::Closure` via `wire_callable`),
  isolating captures at submit + running the ref/generator airlock enforcement identically on serial and
  M:N. (Originally the cooperative engine crossed by handle to mirror the tree-walk `interp` oracle;
  that oracle was removed and the by-handle branch was pure serial-vs-M:N divergence — retired, see
  "Executor.submit coop==M:N by value".)

### M-C — implicit nurseries (shipped 2026-06-12)

Every function body and the module top level is an implicit nursery that joins at its `return`/end
(module top joins at program exit); a bare `spawn` is legal anywhere, dropping the explicit `parallel:`
requirement. `parallel:` is demoted to an explicit *inner* sub-nursery for earlier joins. Design:
[`docs/concurrency.md §10`](docs/concurrency.md). Concurrency is now feature-complete (no Tier-E).

- **Join-on-exit.** `return <value>`, fall-through end, and `?` early-return are all join points —
  spawned tasks run FIFO, *then* control leaves; `defer`s run after the join (tasks, then cleanup). A
  `return`/`?` that escapes an *inner* `parallel:` still cancels-and-reports that inner nursery while
  joining the function's implicit one. An uncaught body fault cancels-and-reports the implicit nursery
  (abnormal exit) — identical to an explicit `parallel:` escape.
- **Single join site + zero-overhead gate.** Compiler pre-scans a body for a bare `spawn`
  (`compiler::block_has_bare_spawn`, stops at `parallel:`/nested-fn/`spawn:`-block); if present it emits
  one opening `Op::EnterNursery` and sets `Proto::has_implicit_nursery`. The VM's `do_return` joins it
  (cancel-inner-then-join-implicit, before defers) for `return`/`?`/end. Bodies with no bare spawn emit
  byte-identical bytecode to pre-M-C — perf benches (no spawns) unchanged.
- **Implicit nursery sites.** Function bodies, the module top level, **`spawn:` blocks, and `defer:`
  blocks** each get their own implicit nursery (each runs in its own frame; a bare `spawn` inside binds
  to *that* body's nursery). Joins at the body's own `return`/end.
- **Three-engine parity.** Interp (`call`/`run_block_task`/`eval_top_level` push an implicit nursery +
  `leave_implicit_nursery` join/cancel), cooperative VM, and `--parallel` are byte-identical. Tests:
  `vm::tests::implicit_nursery_*` (3-engine, incl. `_try_preserves_error_value` +
  `_spawn_in_defer_block` review-panel regressions), `interp::tests::implicit_nursery_*`, golden
  `examples/implicit_nursery.chz`. Checker `spawn_at_function_scope_ok` / `spawn_in_plain_fn_ok` /
  `spawn_at_module_toplevel_ok` (the old `spawn_outside_parallel_rejected` flipped); dead
  `nursery_depth` checker field removed.
- **RESOLVED (2026-06-12) — uncaught-fault cancel-report parity:** an *uncaught* fault with un-run
  nursery tasks now prints the cancel-report on the VM's stdout too, matching the interp and the
  `--parallel` engine. Three coordinated fixes in `src/vm/mod.rs`: (1) `unwind_deferred` gained a
  `report_escaped: bool` param — on a genuine fault (passed `true` from the fault-unwind arm; `false`
  from the two B3.4-cancel paths) it now cancels-and-reports each discarded frame's escaped nurseries
  **before** that frame's `defer`s run, matching the interp order (`exec_parallel` /
  `leave_implicit_nursery` report as the body unwinds, then `finish_frame` runs defers); the old
  `_ => return Err(rte)` uncaught arm reported nothing. (2) `drain_escaped_nursery` now reports
  **per-nursery** (innermost-first), not one combined line — two stacked nurseries → two lines, not
  `2 pending` (also fixed a latent recover-caught combine divergence). (3) the MODULE top-level
  nursery is preserved (`nursery_len + 1` floor): an uncaught *top-level* fault stays silent on both
  engines (it joins only on clean program exit). Review-panel (SRE) caught a defer/report interleave
  divergence the first cut missed; cold pass verified the shared `unwind_deferred` interactions.
  Tests: `vm::tests::uncaught_fault_reports_implicit_nursery` / `_explicit_parallel` /
  `_each_nursery_separately` / `_reports_before_frame_defers` / `_interleaves_report_and_defer_per_frame`
  / `_uncaught_toplevel_fault_does_not_report_module_nursery`, plus `recover_caught_fault_reports_*`.
  Full suite green (1600), three-engine parity.

### Standing decisions & contracts (do not re-litigate)

> **DECISION — do NOT build interp B1/B2 (suspendable tree-walker). Deliberate non-goal.** The interpreter
> stays frozen at the sequential concurrency subset and serves as the differential-testing parity oracle
> for the non-blocking surface (its real value: catching VM / GC / compiler bugs). Suspendable execution
> would need stackful coroutines or a full CPS `eval` rewrite — large, risky, covering a slice the oracle
> does not need. **The VM is the sole concurrent engine.**

- **Parity contract (narrowed, intentional):** the engines agree on the **sequential subset** — all
  *non-blocking* `parallel:` / `spawn` / `Channel` / `Shared` / `Executor` programs (byte-identical,
  parity-tested). **Suspendable concurrency (blocking `recv`) is VM-only by design**: under `--interp` a
  blocking `recv` faults `deadlock` (pinned by an interp test vs the VM golden). This divergence is the
  stated contract, not a bug.
- **Known VM v1 limits (acceptable; not parity issues):** a blocking `recv` reached inside a native
  callback (list HOFs, `sort`, `compare`/`hash`/`str` hooks, `Shared.update`, executor drain, a `defer`red
  call) faults `deadlock` *unless* Path C demotion applies (`recv`/`sleep`/socket under `--parallel`); a
  fiber blocked in an outer nursery *is* woken (D0 cross-level wake-marking, common case works); the narrow
  circular case (its unblocker is an outer sibling the inner scheduler must run) is **RESOLVED under
  `--parallel`** by the M:N flat scheduler (see the cross-nursery section above) but **still faults
  `deadlock` on the cooperative `run`/`--interp`** engines (the cooperative flatten is a separate, later
  commit). Independent/normal multi-level nesting (no shared channel) RUNS under `--parallel` and matches
  coop (the old "2+ enlisting levels" gate is gone). Residual M:N limits: a genuinely-CONTENDED shared
  channel across nested nurseries (2+ live receivers racing ONE channel) is concurrent-divergent BY DESIGN
  (delivery order may differ, or it deadlock-faults — never panics/hangs); the inline outer-body's
  *blocking* recv (case B — wake-side fix only; put blocking work in a `spawn:`); and eager
  (per-connection) nurseries' private sched.
  Fix design + resolution in [`docs/cross-nursery-flat-scheduler.md`](docs/cross-nursery-flat-scheduler.md);
  correct cooperative pattern in `examples/parallel_cross_nursery_ok.chz`.
  Documented residuals: a narrow parked-sibling false-positive under multi-demote; the `Shared.update`
  same-box recv hazard; a saturated-pool queued-task counted live (no-false-positive choice).
- **Use `iter.map`/`iter.filter`/`iter.fold`/`iter.reduce` (chezzi source, `std/iter.chz`)** if a
  callback may block under `--parallel` — they run through VM frames so a blocking `recv` parks. The
  native `xs.map(f)` is the faster non-blocking path (and demotes via Path C if a `recv` blocks in it).

**Permanent non-goals:** interp B1/B2 (above); variadic args, bignum (`i64`-only — every overflow is a
recoverable fault; binary work → the `bytes` (immutable) + `bytearray` (mutable) *sequence* types, both **shipped** — no `byte`/`u8` scalar). **Level-3 dynamic
C-ABI FFI is NO LONGER a non-goal — v1 shipped** (`extern "lib":` scalar calls via dlopen+libffi,
**plus opaque C `void*` handles** via the `ptr` type — `Obj::Ptr`/`Value::Ptr`, `std.ffi.null()`/
`is_null`, untyped + manual-free, `examples/ffi_ptr.chz`; **plus the return-only `str` opt-ins
`owned_str`** (copy + libc `free`, no leak) **and `str?`** (`NULL` → `None`, `examples/ffi_str.chz`);
**plus bidirectional fixed-width integers `int8`..`uint64`** (bind C `int32_t`/`uint32_t`/…;
truncate-on-param / sign-or-zero-extend-on-return, **imported per-name from `std.ffi`** — Chezzi's
first type imports, `examples/ffi_int.chz`);
**plus flat-scalar structs by value** (a Chezzi `struct` of scalar fields ↔ a C struct passed/returned
by value, `examples/ffi_struct.chz`);
**plus `bool` ↔ C `_Bool`** (1 byte — params/returns/struct fields; int-returning predicates like
`isdigit` bind `-> int` + test `!= 0`);
**plus sync scalar callbacks (#4)** (a `fn(scalars) -> scalar` extern param → a libffi closure
trampoline C calls back synchronously, same-thread, scalars only; faults caught + re-raised; both
engines + `--parallel` parity; `src/native/cffi.rs` `CType::Callback` + `Host::invoke_callback`);
nested structs / `str` struct fields / **the rest of callbacks (#4 — stored/cross-thread + pointer-deref
builtins)** / **varargs (#5)** (with design notes + the callback feasibility ladder +
a varargs fixed-arity workaround in `docs/ffi-and-packaging.md §1b`),
a custom user-named deallocator, C-spelling int aliases (`c_int`), and the rich Rust
`Box<dyn Any>` userdata handle still deferred — see "Done" below; forward design for the Rust
userdata Value + the package registry is in
[`docs/ffi-and-packaging.md`](docs/ffi-and-packaging.md)). **`yield`/generators are likewise
no longer a non-goal — complete VM-only support shipped** (see below).

> **`yield`/generators — complete, VM-only (landed on `feat/yield-generators`).** No longer a
> non-goal: a `fn` declaring `-> Iterator[T]` may `yield`; the call returns a suspendable generator
> (a one-shot cooperative coroutine — its own private frame/stack swapped into the VM, resumed by an
> intrinsic `.next()` that the `for`-loop step drives). VM-only: the frozen interpreter rejects
> `yield` (it cannot suspend a native Rust call), so **two-engine parity is waived** for generators.
> `defer`/`spawn`/`parallel:`/`wait:` are checker-forbidden inside a generator. See
> `examples/generators_basic.chz`, the `vm_generator_*` tests, and the `generator_*` checker tests.
> The adapter-struct model over `Iterator[T]` (`examples/iter_adapters.chz`) stays the parity-clean,
> recommended way to write lazy sequences.

---

## Done (newest → oldest)

One bullet per milestone/epic. Full landing detail (TDD notes, review-panel findings, test-count deltas,
branch names) is in the git log.

- **Refactor — a native's behavioural properties now ride its REGISTRY ENTRY, so forgetting one is a
  compile error (2026-08-05, `docs/future.md` §3c option B + the interception fold).** A native's
  properties used to live in string matches far from where it is registered: a 40-name `is_blocking`
  list (with a `strip_prefix('_')` patch bolted on so the W7-8 rename would not silently un-classify
  every `std.fs` syscall), three `"sleep_ms"` arms in `vm/call.rs`, a `name == "connect" || "listen"`
  check, and a `fn_addr_eq` identity test — `sleep_ms` named in 4 files. **A new blocking native that
  forgot the list failed SILENTLY**: no error, no red test, it just pinned an M:N core worker for the
  syscall (the D5 starvation the classification exists to prevent).
  Now `Kind { Inline, Blocking, TimedWait, InterceptIo, InterceptNet }` is a field of every `MEMBERS`
  tuple (192 entries, 14 tables), copied onto `Obj::Native` at bind time and carried through
  `WireValue`/`SnapValue`/`Callee::Native` into `invoke_native(func, name, kind, args, span)`. **No
  native's behaviour is decided by a string comparison anywhere in the VM**, there is no name→kind
  lookup (the kind rides the value — `invoke_native` has exactly one call site), and `is_blocking` +
  the bare-name-uniqueness guard are **deleted**. Bare-name keying was in fact unsound: `std.io::_append`
  (an intercepted opener) and `std.fs::_append` (a syscall) collide, and were kept apart only by check
  ORDER plus a test exemption list.
  Pure refactor — byte-identical behaviour, re-measured on the release binary: `--timeout=200` still
  aborts a `sleep_ms(3000)` at top level, in a nursery, and in an eager `Executor` in ~200 ms each at
  `CHEZZI_THREADS=1/2/4/8`; 4×`process.cmd("sleep 0.3")` still overlap on one core worker (305 ms
  offloaded vs 1209 ms on `--serial`); `io.create`/`stdout`/`buffered` + `net.listen`/`connect` still
  allocate their handles on both engines. Guarantee demonstrated by dropping one entry's kind →
  `expected a tuple with 3 elements`. Suite 3820 green.
  **Found by the conversion, not fixed there:** `fs.stat`/`fs.walk` were never in the old list, so they
  pinned a worker — the predicted silent omission, already in the tree. Preserved as `Kind::Inline`
  (behaviour-identical) with a `BUG PRESERVED` comment + a test pinning the state, filed as `gaps.md`
  **W7-19** and fixed the next day (below).

- **Fix — `fs.stat`/`fs.walk` no longer PIN an M:N core worker (2026-08-05, `gaps.md` W7-19).** They
  were the only two of `std.fs`'s seventeen members outside the blocking set, so their syscalls ran
  inline on a core worker — `walk` for an entire tree walk — the D5 starvation the set exists to
  prevent. Both ancestors hand the worker off here (Go's runtime releases the P on a blocking syscall;
  CPython drops the GIL around `os.stat`/`os.walk`). Now `Kind::Blocking`, after the off-heap-safety
  proof the gap asked for: both take their path through `Host::arg_bytes`, and both returns are
  primitive `NativeRet`s already crossed by members that offload today (`_list_dir` returns the same
  `Ok(List([Bytes…]))`, `process.run`/`run_args` the same `Ok(Struct{…})`). Measured at
  `CHEZZI_THREADS=1` on a 121k-entry tree, paired against a binary built from the same commit with only
  these two entries reverted: a sibling fiber's worst scheduling gap **136–139 ms → 38–41 ms**, and 4
  concurrent `fs.walk`s **814–825 ms → 449–469 ms** (1.8× — they overlap on the dirty pool instead of
  serializing). The ~39 ms residual is *result lowering*, not the syscall: building the 121k-path list
  as heap objects needs the `Vm`, so it stays on the core worker (it falls to 9–10 ms on an 18k-entry
  tree). `every_syscall_module_member_is_blocking` is now exception-free and is the fence that pins
  the classification; a new `tests/chz` case runs `fs.stat` inside a nursery to cover the offloaded
  `Struct` round-trip (a correctness fence — it passes under either `Kind`, which the review made the
  comment say out loud; `fs.walk` from a fiber was already covered). Suite 3821 green.

- **Fix — an intrinsic protocol method on a built-in is now CALLABLE (2026-07-25, bug-hunt wave-6 W6-3,
  P0).** `fn total[T: Add](xs: List[T], zero: T) -> T` with `acc.add(x)` passed `chezzi check` and then
  faulted `type int has no method 'add'` on BOTH engines — the idiomatic Rust/Go generic shape, broken.
  The checker grants built-ins ~12 protocol conformances *intrinsically* (no user method), but the VM
  hand-intercepted only 2 of them (`compare`, `str`); everything else — `add`/`sub`/`mul`/`div`/`mod`/
  `neg`/`hash` on `int`/`float`, `hash` on `bool`/`str`/`bytes`/a zero-field struct, the arith set +
  `compare` on a numeric `newtype`, `index`/`set_index`/`slice` on `list`/`map`/`str`/`bytes`/`bytearray`
  — fell through to `has no method`. Also reachable without generics via a protocol-typed value
  (`x: Hashable = 5`). Parity-blind (byte-identical on both engines), and the repo's **dominant defect
  class**: a fix applied to SOME arms of an N-way set.
  Fix: one `Vm::intrinsic_proto_method` (`src/vm/call.rs`) that **delegates** to the exact primitive each
  operator form already uses — `arith` (which covers the numeric-newtype grant for free via
  `newtype_arith`), a newly extracted `Vm::neg_value` (`Op::Neg`'s body, now single-sourced in
  `src/vm/arith.rs`), `hash_value` (literally the Map/Set key hash, so `x.hash()` can never disagree with
  `m[x]`/`s.has(x)`), `compare`, and `get_index`/`set_index`/`get_slice` (with the `Option[int]` → raw
  `Nil`/`Int` unwrap `Slice`'s signature needs). Nothing reimplemented ⇒ `a.add(b)` ≡ `a + b` and
  `c.index(k)` ≡ `c[k]` by construction, fault text included. Wired at 5 **miss** sites (inline scalar,
  the merged built-in-container dispatch, struct, newtype, and the catch-all where a boxed `BigInt`
  lands), so a user method always wins and an ordinary struct method call pays **zero** added cost
  (`do_method_call` stays a Tier-1 perf target); benches re-measured within run-to-run noise.
  **The ratchet, worth more than the fix** — keyed on **(protocol × receiver KIND)**, because that is the
  axis W6-3 actually failed on (`compare`/`str` WERE paired; their interceptions were just type-gated
  narrower than the grant set, which a protocol-keyed table cannot express). Three layers:
  (1) `satisfies_args_d`'s success type is `checker::proto::Grant`, a token with a private field, so a new
  early-out written the way every pre-existing one was — a bare `return Ok(())` — **does not compile**; the
  author must pick `grant_intrinsic` (registers the grant) or `Grant::no_intrinsic_method`;
  (2) `grant_intrinsic(protocol, ty)` `debug_assert`s that `(protocol, intrinsic_recv_kind(ty))` has a row
  in `INTRINSIC_PROTO_METHODS` (51 paired rows) or `INTRINSIC_UNPAIRED` (0 carve-out rows — W6-3b emptied it; registering one is still the only legal way to ship an unpaired grant);
  (3) `vm::tests::intrinsic_grants_all_have_vm_arms` sweeps the whole **(protocol × kind) cross product**
  (165 cells): the accepted-cell set must equal the registered rows, then every paired row's generated
  call probe RUNS on both engines. Verified RED both ways: a bare `Ok(())` grant fails to compile, and
  widening `Comparable` to `bytes` (which the earlier protocol-keyed ratchet passed) now fails the suite.
  Three carve-outs FILED rather than silently shipped: `Iterator`→`next` on a raw collection (**W6-3b**,
  stateful, no cursor position — **since FIXED, 2026-07-26: the grant was narrowed to real cursors and
  `INTRINSIC_UNPAIRED` is now empty; see the top entry**), `compare` on a **NaN** operand (**W6-3c** —
  first shipped as an explicit recoverable fault instead of W6-3's own `has no method` symptom; **now
  FIXED (2026-07-26)**, it answers `sort()`'s total order — see the wave-6 tail entry below), and a
  numeric `newtype` that
  DEFINES `add`/`compare` (**W6-3d** — the method form gets the user method, the operator form the
  underlying's native op; reqs "≡ the operator" and "never shadow a user method" genuinely conflict there).
  Tests: `tests/chz/spec/intrinsic_proto_methods_test.chz` (20 `test fn`, operator-equivalence AND
  fault-message equality via `recover:`, user-method-wins controls, plus the W6-3d divergence pin, the
  NaN total-order pin and the Iterator-is-a-cursor pin),
  serial==M:N. Docs: `docs/gaps.md` (W6-3 FIXED + new W6-3b/c/d), `docs/spec.md`, `docs/syntax.md`.
- **Diagnostic — recursive *local* fn crossing the airlock (2026-07-18, bug-hunt).** A nested (local)
  recursive `fn` sent across a task boundary used to fault with the misleading `maximum structural depth
  (10000) exceeded (cyclic data structure?)` — there is no cyclic *data*, just the compiler letrec's
  self-cell making the closure's capture graph self-referential (`Closure h -> Cell -> h`), tripping the
  generic depth guard. The two closure-serialization arms (`to_wire_depth` / `to_snap_depth`,
  `src/vm/sched.rs`) now scan the crossing closure's capture graph for its own handle (new
  `graph_reaches_handle`, sibling of the Task-2b ref-capture scan) and raise a clear, **recoverable**,
  byte-identical-on-both-engines error: `a recursive local fn cannot be sent across a task boundary — hoist
  it to module scope (a module-global recursive fn is sendable)`. Fires at every airlock arm (`spawn:`
  block, `spawn f()` callee, `spawn f(g)` arg, `Channel[fn].send`). **Diagnostic only — actual
  recursive-local-fn sendability stays DEFERRED** (a risky VM change past the JIT freeze); the fix is to
  hoist the fn to module scope (crosses as a plain `Func`, no capture, sendable). Genuine cyclic *data*
  still reports the depth message (regression-locked). +2 tests (`airlock_recursive_local_fn_clear_diagnostic_both_engines`,
  `airlock_module_global_recursive_fn_control_sends`). Docs: `docs/concurrency.md` §7, `docs/gaps.md`.
- **QoL — int-const if/match-EXPRESSION branch widens to float (2026-07-17).** A bug-hunt papercut:
  `x := if c: 1 else: 2.5` was a compile error while the equivalent list literal `[1, 2.5]` coerced fine —
  an inconsistency in where the untyped-int-constant→float peephole applied. Extended the SAME
  `literal_numeric_mix` mechanism (float-const sibling → widen the int-const siblings) to if/match-expression
  tail branches, on BOTH sides under one predicate: checker `branch_widen` (in `infer_if_else`/`infer_match`)
  widens the branch type, compiler `compile_if_expr`/`compile_match_expr` emit `Op::CoerceFloat` on the
  int-const branch — identical `untyped_int_const` guard, zero drift. Sound (proven no int-under-float,
  both engines: `if_match_expr_int_float_widen_parity`). A TYPED int branch still rejects (the V1 hole
  boundary), and multi-`return` inference is UNTOUCHED (still conflicts — a separate join, not a widening
  sink). Consistent with `[1, 2.5]`; docs in `syntax.md` (if-expression) + `spec.md` (widening contexts).
- **P0 fix — a cancelled task's `defer` silently did not run on M:N (2026-07-14).** Pre-existing
  scheduler race. A cancel trip and its `cancel_drain` (which requeues the scope's PARKED fibers so they
  unwind) sit two core-lock acquisitions apart; an idle worker's `take_runnable` landing in that gap saw
  `running == 0 && runnable == 0 && parked_n > 0 && done < total`, called the teardown a **deadlock**, and
  `flag_deadlock` dropped the still-parked sibling **without `unwind_deferred`** — its `defer`s never ran.
  Invisible: `reduce_task_slots` ranks `Exit > Fault > Deadlocked`, so the real sibling fault surfaced and
  the lost `defer` was the only symptom (`defer` is the language's ONLY cleanup mechanism — an unclosed
  file, an unreleased lock, silently). Fixed with a **cancel-teardown veto** in `MnSched::is_deadlocked`
  (`SchedCore::any_incomplete_scope_cancelled`): a scope with `cancel` set and `done < total` is
  mid-teardown, not deadlocked. Put at the predicate, not at the one seam that reported it, because
  **three** seams trip a cancel and drain in a later lock acquisition. The veto alone was not enough —
  the two abort seams (`abort_enlisted_scope`, `abort_eager_nursery`) *cleared their own* deadlock veto
  (`awaiting_builder` / `body_open`) before arming this one, so they now trip the cancel FIRST
  (`MnSched::trip_scope_cancel`, a store **under the core lock** — a bare `Relaxed` store outside it had
  no synchronizes-with edge to the worker evaluating the predicate). Two more holes in the same
  invariant, both fixed here: the **panic-fault** path (a worker-VM panic → `Vm::panic_outcome`) never
  tripped the cancel at all, so the requeued siblings re-parked and the scope quiesced *uncancelled*;
  and the netpoller's `register` gated the park on the **outermost** nursery's flag, so a fiber of a
  cancelled INNER scope could park on an already-swept poller (which would have made the veto permanent
  → deadlock detection disabled sched-wide) — `poll_park_offload` now hands it the per-scope cancel, and
  the dead sched-level `MnSched::cancel` field is deleted. Transient by construction (park/park_wait/
  poller-register all refuse to park a cancelled fiber, and every trip is followed by a notifying
  `cancel_drain`), so it can never become a hang; genuine deadlock detection is untouched.
  `parallel_defer_runs_on_cancelled_sibling`: **14/200 failures → 0/200** under CPU contention (35/200 on a busier box);
  `parallel_defer_runs_when_enlisted_nursery_escapes` covers the escaped-enlisted-nursery seam; the
  invariants are pinned by `mnsched_cancelled_scope_with_parked_fibers_is_not_deadlock`,
  `panic_fault_trips_the_scope_cancel` and `poll_park_rejects_cancelled_inner_scope`.
  See `docs/gaps.md` **N4**. Two related holes found while verifying it, both pre-existing, both left
  open with their own entries: **N5** (a GENUINE deadlock also tears parked fibers down without
  `unwind_deferred`, so it skips `defer`s too — but both engines agree, so it is a known limit, not a
  parity break) and **N6** (`--serial` does **not** run a PARKED task's `defer` on a sibling fault — it
  abandons the parked children at `run_child(i)?` — a real **serial ≠ M:N divergence**, uncovered by the
  parity suite, where the *oracle* is the wrong engine; fixing it moves serial's fault-path output
  ordering, so it is its own task).

- **R1 — the `bytes` native seam + binary IO/sockets; B1 now FIXED (2026-07-14).** `bytes`/`bytearray`
  shipped complete below the native seam (lexer/checker/VM/GC/airlock), but the seam itself was
  `str`-only, so **no native fn could accept or return them**. Widened in three places
  (`src/native/mod.rs`): `NativeRet::Bytes(Vec<u8>)` (lowered in `Vm::lower_native` to `Obj::Bytes` —
  the immutable form; a caller wanting mutation writes `bytearray(b)`), a defaulted-to-error
  `Host::arg_bytes` (implemented on `VmHost`; `bytes`-ONLY — a `bytearray` is not assignable to a
  `bytes` sink (7b29552: a mutable buffer aliased as immutable `bytes` is the hole that rule closes),
  so a caller converts with `bytes(ba)`, an explicit copy exactly like CPython's `bytes(ba)`),
  and `NativeArg::Bytes` + `OffloadHost::arg_bytes` so a **blocking** bytes native still offloads to
  the dirty pool instead of pinning a core worker (D5). No new type, no heap obj, no GC/airlock work.
  `value_to_native_ret` deliberately gets no bytes arm (it writes C's return register; the checker
  restricts callback returns to C scalars). Consumers wired: `io.read_bytes(path) -> Result[bytes]` /
  `io.write_bytes(path, data) -> Result[nil]` (binary whole-file IO — `read_file` decodes UTF-8 and so
  hard-failed on any binary file; it now says `use io.read_bytes for binary files`; the 64 MB
  `MAX_READ_FILE_BYTES` cap applies read-side only, as for `read_file`), `crypto.sha256_bytes(b)`
  (hash a file / a socket payload), `encoding.base64_encode_bytes` / `base64_decode_bytes` (the
  arbitrary-binary base64 round-trip), and `Socket.read_bytes(n[, timeout_ms]) -> Result[bytes]` /
  `Socket.write_bytes(b[, timeout_ms]) -> Result[int]` (`src/vm/netio.rs` — **B1's honest fix**: binary
  sockets work; `read_bytes` returns AT MOST `n` bytes and DRAINS the carry first, so the bytes the str
  `read`'s sticky `Err("invalid utf-8 …")` refused to deliver are recoverable instead of forcing a
  `close()`; the str `read` keeps its documented decode contract, unchanged). **Not done (out of
  scope):** `std.request` binary bodies (ureq `into_string()` needs its own reader plumbing → "download
  a file" stays blocked on *request*, not on R1), gzip/zlib, `NativeRet::ByteArray`, and any
  `Writer`/file-handle type (that is R2). Docs: `docs/stdlib.md`, `docs/gaps.md` (R1 DONE, B1 FIXED,
  dependent bullets re-graded), `std/{io,net,crypto,encoding}.chz`.

- **`Socket.read` no longer CORRUPTS data — B1 MITIGATED (2026-07-14).** `src/vm/netio.rs` decoded every
  socket chunk with `String::from_utf8_lossy` at TWO sites (the fast path + the in-callback demote
  poller), so the `str`-only seam (`read -> Result[str]`, `std/net.chz`) silently produced U+FFFD: any
  **binary** payload became garbage, and — worse — **valid UTF-8 text** was mangled whenever a multibyte
  codepoint straddled a `read(n)` boundary, i.e. the ordinary read-in-a-loop idiom (`read(1)` over
  `"héllo"` → `h\u{fffd}\u{fffd}llo`). The runtime lied about the *data*. Both sites now route through
  ONE guard, `Vm::decode_carry`, which splits the two cases `Utf8Error` already distinguishes:
  **(a) truncated tail** (`error_len() == None`) — the ≤3-byte incomplete codepoint is RETAINED on the
  `Arc`'d `SocketCore` (`carry`, so it survives the would-block park's `ip`-rewind re-execution) and
  prepended to the next read, so byte-at-a-time reads of valid text reassemble **exactly**; **(b)
  genuinely invalid bytes** (`error_len() == Some(_)`) — a recoverable
  `Err("invalid utf-8 on the socket: …")` that is **non-destructive and sticky** — the valid text before
  the bad byte is delivered as a normal `Ok`, the undecodable bytes STAY carried, and every later read
  re-errs identically, so a log-and-continue caller cannot silently shred the stream (an `Err` that eats
  the chunk it already took off the fd is just silent data loss wearing an `Err`); it must `close()`. An
  incomplete codepoint left when the peer closes is `Err("invalid utf-8 at eof: …")`, never a silent drop.
  **Contract changes:** `n` bounds the NEW bytes off the fd, so a `read(n)` may return up to `n + 3`
  bytes; a chunk holding no complete codepoint re-reads rather than returning `Ok("")` (that is the EOF
  sentinel — returning it mid-`é` would silently truncate every `while chunk != "":` loop), so **a read
  blocks until it has at least one whole character** — the Go `bufio.Reader.ReadRune` / Python
  text-mode-socket contract, escapable via `timeout_ms` or the peer's close. `timeout_ms` bounds the WHOLE
  call on EVERY path and the carry is kept across the timeout `Err` (the bytes are still owed). `close()`
  stays `-> nil` — a leftover tail at close is dropped, the EOF error surfaces on the READ that sees the
  close. Review-caught corners of the retry loop, all fixed here: **(i) `read(0)`** (a caller-computed
  `read(want - have)` that lands on 0) is a **no-op `Ok("")`** — it never touches the fd, because a
  zero-length `Read::read` answers `Ok(0)` unconditionally and would spin the retry loop forever against a
  pending carry — but it is taken AFTER the closed-socket check, since the stream lock's `None` arm is the
  only closed-fd detector on the read path and `Ok("")` there is indistinguishable from EOF; **(ii) the fd
  read and the carry update are ONE critical section** (the `carry` lock is taken OUTER, `stream` inner) —
  split, two fibers sharing a socket (the `spawn handle(conn)` idiom) could decode out of wire order and
  error valid text as "invalid utf-8"; **(iii) the read's deadline is LATCHED on the fiber**
  (`Vm::poll_deadline`, swapped in `FiberCtx` like `poll_timed_out`) — a park rewinds `ip` and re-executes
  the op, so recomputing `now + timeout_ms` per park let a byte-per-(timeout-ε) dribbler keep a
  `read(n, ms)` alive forever — **and it is now threaded into `Vm::demote_block_socket`** (`sched.rs`),
  which took no deadline at all: an in-callback read (`native_reentry > 0` — a `list.map`, a
  `Shared.update`) waited on fd readiness forever, and a demoted op is accounted `inflight`, which VETOES
  the deadlock predicate — so it hung with no fault and no `Err("timeout")`; **`write`/`write_bytes` and
  `accept` now latch the same way (N2, 2026-07-15)** — extracted to `Vm::socket_write`/`Vm::listener_accept`
  and routed through `poll_deadline` + the shared `Vm::drop_poll_latch` clear, so they no longer recompute
  `now + timeout_ms` on a re-park (a robustness/consistency fix — `write` is architecturally single-park so
  the re-arm was only reachable on a spurious wake); **(iv)** a `read` that TOOK bytes but completed no
  codepoint returns `Err("incomplete utf-8: …")`, not `Err("timeout")` (which is documented as *nothing
  arrived*) — the bytes are off the wire and retained, and saying "deadline expired" about a read that
  consumed data is the same lie-about-your-data class B1 exists to kill. **This classification is now
  consistent across ALL timeout paths (N3a, 2026-07-15)** — poll-once, the netpoller park, and the
  in-callback demote — via a fiber-latched `Vm::poll_partial` (the twin of `poll_deadline`) consulted at
  both timeout sites through the shared `Vm::sock_incomplete_err`. Also: the decode's hot path (no carry, valid chunk) decodes the fd buffer BORROWED
  (`Cow`), so it costs one `String` alloc — exactly what `from_utf8_lossy(..).into_owned()` cost, no
  regression on the IO path. **Known v1 limit (unchanged): binary sockets are UNSUPPORTED** — they need
  `Socket.read_bytes`/`write_bytes`, which need the `bytes` native seam (`docs/gaps.md` R1, the honest
  fix). B1 is downgraded from *silent corruption* to *a clear, recoverable limitation*, not closed. Pinned
  by nine M:N tests (`net_read_reassembles_split_codepoint_over_parallel`,
  `net_read_invalid_utf8_errs_not_replacement_chars`, `net_read_incomplete_tail_at_eof_errs`,
  `net_read_zero_with_pending_carry_returns_empty_not_spin`, `net_read_zero_on_closed_socket_errs`,
  `net_read_shared_socket_two_fibers_decode_in_wire_order`,
  `net_read_poll_once_mid_codepoint_errs_incomplete_not_timeout`,
  `net_read_timeout_bounds_whole_call_across_codepoint_parks` (N3a: asserts `incomplete utf-8`),
  `net_read_timeout_bounds_the_in_callback_demote_path` (N3a: asserts `incomplete utf-8`),
  `net_read_partial_timeout_then_clean_timeout_is_not_incomplete` (N3a stale-latch clear guard),
  `net_write_timeout_when_buffer_full` (N2)); the serial engine's net path (immediate
  "requires the --parallel engine" `Err`) is untouched, so no parity divergence.
- **stdin is SHARED by every task (Go/Python) — the false EOF is dead (2026-07-14).** stdin belonged to
  the ENTRY task: every other task was handed `Stdin::Empty`, so `io.read_line()` / `io.input()` inside a
  `spawn:`/nursery task or an `Executor.submit` task returned `None` — a **false EOF**, while the entry
  task still had unread lines queued. That rule existed only to protect the byte-identical serial==M:N
  parity oracle (a shared consumable stream makes which-task-gets-which-line scheduling-dependent) — the
  same fake determinism the interactive-CLI milestone just removed from stdout. **The oracle bends; the
  language does not.** Now: ONE stdin source, shared by every task, exactly like Go's `os.Stdin` and
  Python's `sys.stdin` — any task may read it; a line goes to **exactly one** task (never duplicated,
  never dropped); **which** task gets a given line is **nondeterministic** on both engines (want
  deterministic distribution? entry task reads and fans out over a `Channel[str]` — the same "order it
  yourself" answer as concurrent `print`); `None` means genuinely exhausted (a real EOF still EOFs).
  Killed at **every** task-entry seam, because an invariant enforced at one seam is not enforced (that is
  how the Executor divergence survived five hunt waves): `FiberCtx::stdin` + its `swap_ctx` park DELETED
  (the `spawn:`/nursery fibers now read the one `Vm::host.stdin`), `spawn_worker` shares the handle
  instead of `Stdin::Empty` (the single M:N funnel — nursery + Executor pool drain), and the cooperative
  `Executor` inline drain's stdin park (65c2e42) is **reverted** — it made both engines agree on the
  WRONG semantics, and consistent-and-lying is still lying. Sharing is by **handle, not copy**:
  `Stdin::Lines` became an `Arc<Mutex<VecDeque<String>>>` (a by-value clone would hand every worker its
  own copy of every line — delivering each line N times, worse than the bug being fixed) and `Stdin::Real`
  stays a unit variant over the process-global, internally-locked `std::io::stdin()` (one `read_line` =
  one whole line, atomic across the M:N worker threads; never a per-worker `BufReader`, which would steal
  bytes into a private buffer and drop lines). `Stdin::Empty` survives only as a legitimate host config
  (an embedder with genuinely no stdin) — hence the golden/parity suite stays byte-identical green.
  Pinned by `parity_{spawned,executor}_tasks_share_stdin_exactly_once` (both task-entry families, both
  engines: 3 lines, 2 tasks + entry ⇒ the stdout line MULTISET is exactly the 3 lines, each once, no
  `eof` — asserted as a multiset, never an exact stdout, because the assignment is nondeterministic BY
  DESIGN and an `assert_eq!` here would be a flake built on purpose), `worker_shares_the_one_stdin_source`
  (a line the worker consumes is GONE for the parent — proves shared, not cloned) and the real-binary
  `task_reads_piped_stdin_{mn,serial}` (the `Stdin::Real` path, piped stdin, both engines). **New v1
  limit:** `read_line`/`input` are deliberately outside `is_blocking` (off-heap `OffloadHost::read_line`
  is `unreachable!`), so a task blocked in a read now **pins an M:N core worker** (K blocked readers ⇒ K
  pinned workers) — previously impossible, since tasks got an instant EOF. Accepted + documented
  (`docs/gaps.md`); offloading stdin reads is its own milestone.

- **Interactive CLI — `chezzi run` STREAMS stdout; the buffered sink stays for tests (2026-07-13).**
  `src/main.rs` used to capture the whole run into a `String` and `print!` it once, after the VM
  returned: a prompt never appeared before its `read_line`, a hung/killed program printed NOTHING, a
  long-running program was silent until exit, and a spawned task's log was invisible until its nursery
  joined (for a server: never). Now the stdout/stderr sink is selected by `HostConfig::stream` (default
  `false`): **every lib helper, golden and parity test keeps the BUFFERED sink unchanged** (per-task
  buffers + task-order flush → byte-identical serial-VM == M:N-VM; zero test edits), while `chezzi run`
  sets `stream = true` and each `print` becomes ONE `write_all` on the real stdout (line-atomic across
  tasks). In stream mode the per-task buffers just stay empty, so the scheduler is untouched.
  New `std.io` surface: **`flush()`** and **`input(prompt)`** (= print prompt, flush, read line →
  `Option[str]`). The write itself happens on a **background writer thread per stream** (`vm::stream`),
  which `write_all`s + `flush`es each message: a fiber must never sit in `write(2)` — an inline blocking
  write on a core worker means one stalled reader starves every other task (the D5 `is_blocking`
  invariant) — and the streamed handles are **unbuffered**, so a `print(x, end="")` progress marker
  appears (and survives a kill) with no `io.flush()`. Nothing in the VM ever *waits* on a writer thread
  (`flush`/`read_line`/`input` only queue), so a stalled consumer cannot pin a worker either; `flush()`
  is consequently a no-op that exists because it is the portable idiom. A writer thread **never calls
  `std::process::exit`** (this is library code; two threads racing libc `exit(3)` is UB, and a thread
  that kills the process discards the run's real outcome): it records, and the VM raises an ORDINARY
  runtime fault at its next `print` (`stdout closed (broken pipe)`). Policy — a closed stdout reader
  (`| head -1`) fails the run non-zero with a trace on the still-live stderr (Python raises
  `BrokenPipeError` identically), and an endless printer stops instead of spinning on a dead pipe. A
  program's **LAST** print into a just-closed pipe has no *next* print site to fault at, so the in-VM
  `stream_halt` never fired — the exit status used to be a nanosecond RACE (exit 0 if the VM outran the
  writer's EPIPE, dropping the bytes and reporting SUCCESS; 1 if the writer won). **Fixed (N1, 2026-07-15):**
  `cmd_run` re-checks `vm::out_dead_reason()` AFTER `flush_stream()` (which blocks on the writer ack, so
  `OUT_DEAD` is final) and fails deterministically — outranked by a non-broken-pipe `stream_error` and by
  `os.exit`, skipped when the VM already faulted (no double-report). Pinned by
  `last_print_into_closed_pipe_is_deterministically_nonzero_{mn,serial}` (+ `fully_drained_output_stays_success_*`
  for the no-regression clean case). The
  dead pipe must NOT borrow the `os.exit` channel to halt: that channel outranks a fault everywhere
  (`run_file_with_entry` discards the `Err` when `pending_exit` is set; `classify_mn_outcome` ranks
  `Exit` above `Fault`), so the first cut of this milestone turned a *faulting* run under `| head -1`
  into a silent **exit 0 with no trace** — a crashing program reporting SUCCESS to CI. Caught by the
  review panel, fixed before merge, pinned by `fault_under_broken_pipe_is_not_success_{mn,serial}`; any other stdout errno prints
  `chezzi run: cannot write stdout: …` and exits non-zero (a `> /dev/full` run can no longer report
  SUCCESS with no output); a **stderr** write failure is swallowed (diagnostic channel — a dead `2>`
  reader must not kill a healthy program). A spawned task's stdin was made `Empty` on **both** engines
  here (`swap_ctx` parks the entry task's stdin), closing a serial-vs-M:N divergence `read_line`/`input`
  inside a `spawn:` would otherwise have — **SUPERSEDED 2026-07-14** (see the shared-stdin entry above:
  the `Empty`-for-tasks rule was a false EOF; stdin is now one source shared by every task).
  The task-order flush is now documented as a **test-harness**
  property, not a user guarantee: cross-task print order is nondeterministic on both engines
  (Python/Go/Rust-identical); join and print the results yourself if you want order. Perf:
  `benches/run.chz` unmoved (all within noise); an ad-hoc 200k-line print loop goes **0.048 s →
  0.101 s** (~2.1×: one write syscall per line + the queue handoff — a `BufWriter` would hide the
  syscall but break "a killed program retains its output"). New real-binary suite `tests/interactive.rs`
  (34 tests, both engines: prompt-before-stdin, killed-program output, partial line visible with no
  flush, spawned-task print before join, order-insensitive concurrent lines, `from std.io import input`,
  broken-pipe clean exit, fault-after-broken-pipe still reported, last-print-into-closed-pipe
  deterministically non-zero (N1) + fully-drained clean run stays success, unwritable stdout reported,
  unwritable stderr does not kill the run, both pipes closed terminates, stalled reader does not starve
  — with or without `io.flush()`).

- **`regex.Match.start`/`.end` are now CODEPOINT offsets (observable stdlib behavior change).** They
  were the `regex` crate's raw **byte** offsets while Chezzi slicing/indexing is codepoint-based and
  there is no byte-indexed slice — so on non-ASCII input `s[m.start:m.end]` silently produced the
  wrong substring (`"héllo"` + `l+` → `"lo"`, want `"ll"`), with no fault. Converted at the single
  Match-construction seam (`src/native/regex.rs` `match_from_caps`), matching Python's `re`, which the
  module otherwise mirrors. New invariant, asserted in the tests: `subject[m.start:m.end] == m.text`.
  ASCII programs are bit-identical (byte == codepoint); a non-ASCII program doing arithmetic on
  `.start`/`.end` changes result — it was wrong before. Closes pre-JIT wave-5 audited residual #2.
  The conversion is **linear in the subject across a whole `find_all`** (one ascending byte→codepoint
  `CpCursor` over `captures_iter`'s monotonically increasing spans) — a from-zero prefix rescan per
  match would have been O(n·m), turning a document scan into a hang; pinned by a timing test
  (200k matches / 400k chars: 2.9s quadratic vs ~0.1s linear, debug).
- ✅ **A module bind that shadows a same-named USER type now NAMES the collision** (2026-07-13) — wave-5 audited residual 4, **downgraded from a soundness gap to a message fix**. `import lib.Point` + `struct Point` puts the module in the VALUE namespace, where it beats the ctor in expression position — but unlike the reserved-name case (which *silently destroyed* the builtin, hence the `is_reserved_module_bind` gate), a shadowed USER ctor is a **hard type error at the call**, so no program can run wrong, and `import lib.Point as pt` is the cure — i.e. ordinary Python-style shadowing, plus a diagnostic Python does not give. The only real defect was that the diagnostic was `module Point is not callable`, which never said where the ctor went. The not-callable arm (`src/checker/expr.rs`) gained a `Ty::Module` case gated on the bare-key type tables (`self.structs`/`self.enums`, so it is module-prefix-key correct): `module bind 'Point' shadows the same-named type 'Point' — alias the import: \`import ... as point\``. Checker-only, no compiler/VM change → parity- and perf-neutral. +1 test RED-first (`module_bind_shadowing_user_type_names_the_collision`): struct ctor + enum ctor collide, the alias escape hatch reaches BOTH the module and the ctor, and a module bind with no same-named type keeps the generic `is not callable` error (no over-rejection). A real **module namespace** (module names legal only in field position) remains the principled fix and is NOT planned — it is a resolver change that buys only the loss of an alias keystroke. Docs: `docs/gaps.md` (residual 4 downgraded).
- ✅ **Module/import seam: 4 fixes — `std.str` renamed to `std.string`, a reserved module bind is rejected, a nested `import` is a parse error, a from-imported global can't be rebound, and a qualified generic fn's turbofish/hint reach it** (2026-07-13, `auto-task/module-import-seam`) — front-end only (parser + checker + one std file rename); compiler/VM untouched, so both engines stay consistent by construction. **(1) A module bind silently shadowed a builtin ctor.** `import std.str` + `print(str(5))` → `type error: module str is not callable` — importing the documented std module DESTROYED the global `str()` ctor (the whole-module bind puts the last path segment into the VALUE namespace, where it beats the reserved builtin in expression position). Fixed in two parts: **(A)** the std string module is RENAMED `std.str` → `std.string` (`std/string.chz`; `str` was the ONLY std module name colliding with a reserved name — the scalar TYPE `str` and the `str()` ctor are UNCHANGED; matches Python/C++); **(B)** a module bind — aliased OR the un-aliased last path segment — whose name is reserved is now REJECTED (`module name 'int' is reserved (builtin) — alias it: import lib.int as ints` / `import alias 'Ok' is reserved (builtin)`). The gate is `is_reserved_module_bind` = the existing `is_reserved_alias_target` (reserved callables + reserved type names) + `nil` (carved out of the ALIAS gate because an alias binds a VALUE and a value still works as a value — but a MODULE is not a value, so `import lib.nil` would silently retype the `nil` literal) + the extracted `is_builtin_variant` (`Ok`/`Err`/`Some`/`None`, which `is_reserved_type` deliberately excludes so decl-site shadowing keeps working) — 34 names, no new list. Escape hatch: `import lib.int as ints`. The gate is ROOT-SWEPT to every bind that lands in the VALUE namespace, so the `from` form is covered too — a reserved ALIAS (`import x as Ok from M`) *and* a reserved bare MEMBER (`import str from lib.sh`, where a module global/fn named `str` destroyed the `str()` ctor exactly like `import std.str` did) are both rejected (`imported name 'str' is reserved (builtin) — alias it: …`); a reserved TYPE member binds no value and is the LICENSING import of the builtin itself (`import Shared from std.concurrency`, `import ptr from std.ffi`), so it stays legal un-aliased. RESIDUAL (out of scope): a module bind colliding with a USER struct/enum ctor of the same name still wins in expression position. **(2) A nested `import` was a LYING ACCEPT** — an `import` in a fn body/block parsed, checked clean and was a complete NO-OP (even for a module that does not exist): the resolver only scans module-level stmts. It is now a PARSE error (`import must be a top-level declaration`), the third instance of the existing `extern`/`native` depth>1 gate; `<importStmt>` moved from `<simpleStmt>` to `<topLevelStmt>` in `docs/grammar.bnf` + `tests/corpus/reject/import_nested.chz`. **(3) Rebinding a from-imported global was silently accepted** — `import COUNT from lib.st` + `COUNT = 99` wrote a local alias, lost forever, while the qualified `st.COUNT = 5` correctly errored. The SNAPSHOT semantics are correct (verified CPython-identical) and unchanged; only the WRITE path is now rejected (`cannot assign to 'COUNT' imported from module 'lib.st' …`), gated on the name resolving at module scope so a fn-local `:=` shadow stays assignable — and RE-DECLARING the name at module scope (`COUNT := COUNT + 1`) hands it back to this module (`declare` drops the import entry, like it drops a loop-var mark), so the module's own binding stays assignable — and only for VALUE binds — mutating THROUGH a from-imported container (`LST.push(7)`) is a different arm and keeps working. **(4) A module-qualified generic fn whose type param appears only in the return type was unreachable** — `geo.empty_list[int]()` said *"method 'empty_list' takes no type argument(s) (it declares no own type parameters)"* (it plainly does) and the escape hatch `xs: List[int] = geo.empty_list()` leaked an unsolved `List[T]`. `method_has_own_type_params` gained a `Ty::Module` arm, and the module-fn arm of `infer_method_call` now threads BOTH the turbofish and the expected-type hint into `infer_generic_call` (fixing only the first would silently IGNORE the turbofish). Multi-arg qualified TYPE turbofish stays a documented clean parse error. Tests RED-first: parser (nested import) + conformance reject corpus + checker `files_ok`/`files_reject` over a REAL module graph (reserved bind/alias + escape hatch, rebind vs mutate-through vs local shadow, qualified generic turbofish + hint + arity boundary) + two-engine parity runs (`import std.string` with `str(5)` in the same module; `geo.empty_list[int]()`). Docs: `docs/syntax.md` §12 (import is top-level-only, reserved module-bind rule, `from M import X` snapshot semantics, qualified generic-fn turbofish), `docs/grammar.bnf`, `docs/stdlib.md` / `docs/spec.md` / `docs/ffi-and-packaging.md` / `gaps.md` (the `std.str` → `std.string` rename; older dated changelog entries below keep the historical `std.str`/`std/str.chz` spelling).
- ✅ **A range in a value position is now a check-time TYPE ERROR (was: check-OK, then the program could not run at all)** (2026-07-13, `auto-task/range-not-a-value`) — `x := 0..3` type-checked `ok: no type errors` and then died on both engines with `runtime error: a range can only be used as the iterable of a \`for\` loop`, printing **nothing at all** (not even a preceding `print("before")`): a COMPILE error surfaced at run time, so `chezzi check` and the LSP were blind to a whole class of programs that cannot execute. Root cause: `infer_kind`'s `ExprKind::Range` arm (`src/checker/pattern.rs`) laundered `a..b` as **`List[int]`**, so every list operation on it type-checked — the checker's own `y: str = 0..3` diagnostic (*"cannot assign **List[int]** to variable of type str"*) named the bug. The compiler, which is type-blind, then rejected the range everywhere except the 3 positions it can actually lower. **Fix (checker-only, ONE arm → zero compiler/VM/codegen change, so parity- and perf-neutral):** every value position (assign RHS, call arg, collection element, binary operand, method receiver, plain index object, return, generic `Iterator[T]` bound arg, interpolation, pipe) funnels through `infer` → `infer_kind`, so that single arm now reports `a range is only valid as the iterable of a \`for\` loop or comprehension, as a slice receiver, or as a \`match\` pattern — use \`range(a, b)\` to materialize a \`List[int]\`` and yields `Ty::Unknown` (not `List[int]`) to suppress a misleading cascade. **No `Ty::Range` variant:** a range has no runtime value in any engine, so a new `Ty` would ripple through unify/compatible/assignable/Display/sendable/hover and buy nothing — the honest model is "a syntactic form legal in exactly 3 expression positions", which is already what the code assumed. The sanctioned positions never reach `infer_kind` and so keep working untouched: `for_bindings` (`src/checker/sig.rs`) pattern-matches `ExprKind::Range` **syntactically** for BOTH iterable forms (so `for i in 0..10`, `0..xs.len()`, expression bounds, and comprehensions are free, and **laziness is preserved** — `for i in 0..100000000000` still never materializes), and `case 1..5:` is a `Pattern::Range`, a different AST node entirely. Two deliberate bypasses were needed: `infer_slice` special-cases a range receiver → `List[int]` (mirroring the compiler's materialize-then-slice, keeping `(0..10)[::2]` / `(0..5)[::-1]` alive), and `infer_comprehension`'s Channel-drain guard now skips a `Range` clause iter (it re-infers `clause.iter` a second time AFTER `for_bindings`, which would otherwise have rejected `[i for i in 0..3]` — the likeliest over-rejection). Also DELETES the piecemeal prior-art guard in `infer_binary`'s `In` arm (a range RHS is now rejected by the generic arm, so keeping it would DOUBLE-report — pinned by an exactly-one-error assertion). **`List(0..3)` / `Set(0..3)` are deliberately REJECTED, not made to work:** the materialization escape hatch ALREADY EXISTS and is documented — the `range(start, end[, step])` builtin returns a real `List[int]`, so `Set(range(0, 3))` works today; range-aware ctors would be pure surface duplication (a checker whitelist + a compiler special-case + grammar/docs churn) for zero new capability. The hint therefore points at something TRUE TODAY with **zero surface addition**. **INVARIANT (the acceptance bar):** the checker's accepted `Range` set is now exactly {for-iterable, comprehension-clause iterable, slice receiver} — the only three sites that reach a `Range` without `infer_kind` — and that is exactly the compiler's lowered set, so **no check-clean program can reach the compiler's error**. Note `(0..10)[2]` (a plain INDEX, not a slice) was a third instance of the bug nobody had listed; the single arm rejects it for free, and no Index bypass was added (one would make the compiler error reachable again). The compiler's `CompileError` (`src/compiler/mod.rs`) STAYS as a defensive backstop — unreachable from a check-clean program, but the compiler is also driven WITHOUT the checker (synthesized ASTs from difftest/panicfuzz; the `run_capture` / `run_capture_parallel` VM test helpers skip type-checking) — and now carries a comment saying exactly that. **Blast radius: ONE in-repo `.chz`** — `tests/corpus/accept/ranges.chz` (`r := 0..10`, a parse-only corpus that never runs the checker, but it encoded a now-illegal program) rewritten to `for i in 0..10:`, still covering `# rule: rangeExpr`; ZERO changes in `std/*.chz`, `examples/*.chz`, `benches/chz/*.chz`, `judge/` (a corpus grep confirms the only non-`for` range uses are the slices in `examples/range_step.chz` and the comprehensions in `examples/comprehensions*.chz`, all still legal), and `docs/grammar.bnf` needs no edit — `x := 0..3` still PARSES, the rejection is a TYPE error. +2 checker tests RED-first via `entry_rejects`/`entry_ok` (the real `build_graph` + `check_graph` path): 14 escape forms (incl. the repro, `List`/`Set` ctors, `.len()`, `.push()`, `+ [7]`, `== [0,1,2]` — which pins that the permissive `Eq | NotEq` arm still INFERS its operands, the one place the invariant could have leaked — a list/map literal element, a `List[int]` param, an `[S: Iterator[T], T]` bound, plain index, `in`, and the no-longer-laundered `y: str = 0..3`) and 15 must-keep-working boundary positions; + 2 two-engine RUN guards in `src/vm/parity_tests.rs` pinning the COMPILER half (all sanctioned lowerings still execute byte-identically, and `range(0, 3)` → `[0, 1, 2]`, i.e. the hint is true) and the backstop. Docs: `docs/syntax.md` (the range section — a range is not a value + the `range(a, b)` escape hatch; the slice section), `docs/spec.md` (M15 row — the `..`-stays-a-for/match-range design is now actually ENFORCED).
- ✅ **`os.exit(<negative>)` no longer reports SUCCESS to the shell/CI** (2026-07-13, `auto-task/three-runtime-fixes`) — `os.exit(-1)` — the canonical failure idiom — exited the process with status **0** on both engines: `VmHost::request_exit` (`src/vm/mod.rs`) did `code.clamp(0, 255)`, clamping every negative code UP to 0, so a deliberate failure exit was invisible to a shell/CI `$?` check (the real severity: a silent green build). **Fix (one line):** the status is now the POSIX **low 8 bits** of the code (`code & 0xff`) — exactly what `exit(3)` / bash / Python / Go all do: `-1` → **255**, `-2` → 254, `0` → 0, `1` → 1, `255` → 255, `-256` → 0. It is a *mask*, not a clamp, so this deliberately also changes **`os.exit(300)` → 44** (was 255; POSIX `exit(300)` is 44), making the rule total and one-sentence documentable, and idempotent with the CLI's existing `ExitCode::from(code as u8)`. No in-repo `.chz` is affected (`examples/exit.chz` = 2, `examples/parallel_cancel.chz` = 7 — all small positives, bit-identical). +2 tests RED-first in a NEW `tests/exit_status.rs` asserting the REAL PROCESS EXIT STATUS via `Command` on the built binary across `{run, run --serial}` (an in-VM assertion on `pending_exit` could not have caught this bug at all) + an in-VM `vm::parity_tests::exit_negative_code_masks_to_255` pinning the code on both engines through the cross-thread re-store. Docs: `docs/stdlib.md` (the `exit` row), `docs/future.md`.
- ✅ **A re-entrant `.next()` on a currently-RUNNING generator faults instead of silently reporting exhaustion** (2026-07-13, `auto-task/three-runtime-fixes`) — resuming a generator from inside its own body (`holder[0].next()` where `holder[0]` is the generator now executing) answered **`None`** on both engines: a live, non-exhausted generator reporting itself EXHAUSTED — a silently-wrong `Option`, not a fault. Root cause: `generator_next` (`src/vm/exec.rs`) parks `GenState::Done` in the heap object as the placeholder while the generator runs (`mem::replace`), so the re-entrant call hit the `Done` short-circuit and got `None`. **Fix (one guard, 3 lines):** at the TOP of `generator_next` — before the heap borrow and before the state take — `if self.active_generators.contains(&h) { return Err(self.err("generator already running", span)) }`, reusing the resume path's OWN root list (pushed/popped around the run) rather than adding a `GenState::Running` variant that would need explicit write-back on every early-return path. Because the pop happens on **every** unwind path, the guard is **self-clearing** — a generator can never be poisoned as permanently "running" (pinned by a test per path: normal exhaustion, an early consumer `break` then a legitimate resume, a body that faults and is caught by `recover:` then a fresh generator still runs, and a generator driving a *different* generator — no over-rejection). The fault is an ordinary `RuntimeError`: catchable by `recover:`, never a host panic, byte-identical on both engines (Python: `ValueError: generator already executing`). A generator whose body faulted stays **closed**, like Python's (later `.next()` → `None`) — unchanged, now documented. +3 two-engine tests RED-first (the repro printed `1 / reentrant: None / 2` before the fix). Docs: `docs/stdlib.md` ("Iterator cursors & generators" re-entrancy rule), `docs/spec.md` (beside the existing sendability caveat).
- ✅ **`iter.reduce` on an empty list gives a named fault instead of leaking the std module's internal index error** (2026-07-13, `auto-task/three-runtime-fixes`) — `iter.reduce([], f)` faulted with `runtime error (line 72, col 12): index 0 out of bounds (len 0)` — a std-module implementation detail (a line number *inside* `std/iter.chz`, at an index the user never wrote) surfacing as the user-facing diagnostic. **Fix (`std/iter.chz`, pure Chezzi — it must stay Chezzi for `--parallel` callback parking, D5):** a leading `if xs.len() == 0: panic("reduce: empty list with no initial value")` guard (the existing std fault idiom — cf. `std/str.chz`'s `pad_left: fill must not be empty`), matching Python's `TypeError: reduce() of empty iterable with no initial value`. Still a recoverable fault, just an honest message. **Sibling audit of the same leaked-internal-error-on-empty class** (per the fix-at-the-root rule): `reduce` was the ONLY unguarded `[0]`-class index — `str.reverse` and `iter.shuffle` compute `len() - 1` but their `while` guards make `-1` a no-op on empty; `path.basename` gets a 1-element list from `split`; every `collections.chz` index (`min_heap` pop/peek, `deque` peek_front/back) is already `len`-guarded. Nothing else to fix. +2 two-engine tests RED-first (named-message + `recover:`-catchable, with a non-empty `[1,2,3]`→`6` / single-element `[7]`→`7` boundary). Docs: `docs/stdlib.md` (`std.iter` `reduce` entry).
- ✅ **Two check-OK → runtime-fault holes closed: a bound method is no longer readable as a value, and `index`/`set_index` must now be V-coherent** (2026-07-13, `auto-task/checker-method-value-index-coherence`) — both were checker-only accepts of programs the runtime cannot execute; both fixes are checker-only rejects (no compiler/VM change → parity- and perf-neutral). **(1) Bound-method value.** `g := s.get` on a `struct S` with `fn get(self) -> int` type-checked, then faulted on both engines with `no field 'get' on S(n=5)`. Root cause: `infer_field`'s `Ty::Struct` arm (`src/checker/pattern.rs`) fell back to the METHOD table after the data-field lookup missed and handed back a `Ty::Func` still carrying the **un-bound `self` slot typed `Ty::Unknown`** — nothing lowers that (the compiler emits a plain field load), and the `?` self slot **laundered types**: `g("anything")`, `apply(s.get, s)` against a `fn(S) -> int` param, `xs := [s.get, s.get]`, and `g := self.get` inside a method body ALL checked OK and then faulted. The four sibling receiver kinds (enum / newtype / protocol existential / the assign path) already rejected correctly, which pinned the seam exactly. Methods are **not first-class values** (nowhere sanctioned in the docs — an accidental accept, not a feature): the fallback is deleted, so control falls through to the sibling error, with a method-aware hint — `type S has no field 'get' ('get' is a method — methods are not values: call it (\`x.get(…)\`) or wrap it (\`fn(): x.get()\`))`. The misleading follow-on arity message (`'closure' expects 1 argument(s), got 0`) disappears for free: the expr is now `Ty::Unknown` and `infer_call`'s Unknown arm stays silent — exactly ONE diagnostic (asserted). Fields are searched BEFORE methods in the same arm, so the closest neighbour — a genuinely **fn-TYPED field** (`f: fn(int) -> int`, `s.f(3)` AND `g := s.f; g(3)`) — still works, as do method calls, chained calls, calls on a nested field, a field whose name collides with a method, and static/associated CALLS (a bare `S.make` VALUE is not legal today anyway — `unknown name 'S'`). **(2) `index`/`set_index` V-incoherence on a COMPOUND index-assign.** A struct with `index(self, int) -> str` but `set_index(self, int, val: int)` accepted `s[0] += 1`, then faulted with `cannot apply Add to str and int`. `x OP= v` is defined as exactly `x = x OP v` (`docs/syntax.md` §3) — and the compiler lowers it that way (`Dup2 → GetIndex → op → SetIndex`) — so a compound index-assign's LHS must be typed from **`index`'s RETURN**; the direct index-assign path took it from `set_index`'s `val` instead. Fix (`src/checker/sig.rs`, the one `other =>` arm of `check_assign`): a compound now types its LHS through the existing `index_kv` (the READ side) and then requires that value to fit `set_index`'s `val` (and the keys to agree) — the same `IndexSet[K, V]` coherence the bounded `[C: IndexSet[K, V]]` path already enforces — reporting exactly ONE error, `type S does not satisfy IndexSet (index returns str but set_index's val is int)`, with no `cannot apply +=` cascade. A **plain** `s[k] = v` is deliberately NOT gated: it never reads through `index`, so an asymmetric pair (a safe-read `index -> V?`, a widening writer) is sound and keeps type-checking and running exactly as before — `index_set_kv` still returns the `set_index`-derived write slot, so the write side loses no checking (an un-pinned generic receiver still rejects `b[0] = ["oops"]` against a `List[int]` val). Behaviour preserved: coherent user `Index`/`IndexSet` pairs (read / write / compound / negative index / all open+step slice bounds), `index`-only (read-only) and `set_index`-only structs, builtin targets (`m[k] += 1`, `xs[i] += 1`, `obj.f += 1`), and the once-only evaluation of a side-effecting index expr (Python parity). **Blast radius: ZERO in-repo `.chz`** — no example/std/bench/test reads a bound method or compound-assigns an incoherent pair (`examples/slicing.chz`'s `Ring` and the parity `BUF_PROG` are coherent). +11 checker tests RED-first via `check_entry`/`entry_ok`/`entry_rejects` (the real `build_graph` + `check_graph` path) incl. every launder variant, the exactly-one-error assertions and the no-over-rejection guards, + 4 two-engine RUN guards in `src/vm/parity_tests.rs` (coherent `str`-valued IndexSet, asymmetric plain write, index-expr-evaluated-exactly-once, fn-typed-field-as-value). Docs: `docs/syntax.md` (§3 compound-assign ≡ `x = x OP v` + the read-side coherence rule + once-only index eval; §7 methods are not first-class values — call or wrap; §7b `IndexSet` compound-read coherence).
- ✅ **`pad_left` with an empty `fill` no longer LIVELOCKS; a multi-char `fill` no longer overshoots `width`** (2026-07-13, `auto-task/pad-left-empty-fill`) — `"a".pad_left(5, "")` type-checked `ok` and then SPUN FOREVER on both engines with zero output and no diagnostic (stdout is buffered to process exit, so the user saw literally nothing). Root cause: two independent copies of the same unbounded prepend loop — the native method (`src/vm/call.rs`) and the pure-Chezzi free fn (`std/str.chz`) — grew the string until it reached `width`, which an empty `fill` never does. Both now raise the recoverable fault `pad_left: fill must not be empty` **eagerly**, before the `width <= len` early-out, so the diagnostic can't depend on whether padding was even needed (`"".pad_left(0, "")` faults too — deliberate, pinned by a test). Same commit fixes the sibling overshoot bug: a multi-char `fill` is now a repeating cycle **truncated to fit**, so the result is exactly `width` codepoints (`"a".pad_left(4, "xy")` → `"xyxa"`, was the 5-char `"xyxya"`). The never-shrinks early-out (`width <= len` → `s` unchanged) is taken BEFORE the `need = width - len` subtraction in BOTH copies, so a very negative `width` — down to `i64::MIN`, reachable from safe source — can neither overflow i64 (debug host panic / release wrap into a colossal `take`) nor raise Chezzi's checked-`-` `integer overflow in Sub` in the free fn, counting is by codepoint throughout (`chars()`, never byte length or byte slicing, so a non-ASCII fill counts as 1 and no mid-char slice can panic), and the native pad allocation routes through `repeat`'s existing capacity guard (`checked_mul` + `isize::MAX` + `try_reserve_exact`) → a recoverable `string pad capacity overflow` instead of an OOM/abort for a huge `width`. The O(n²) `format!`-in-a-loop is gone (one allocation). **Documented divergence:** that capacity guard has no counterpart in the `.chz` free fn, which cannot probe the allocator — the second sanctioned exception to the byte-identical-alias contract, written down at `docs/stdlib.md`. +6 tests RED-first (the empty-fill test HUNG for the full 900s timeout before the fix, it did not merely fail): 4 native in `src/vm/tests.rs` (empty-fill fault incl. a `recover:`-catch, exactly-`width` multi-char, codepoints-not-bytes, huge-width capacity fault) + 2 free-fn in `src/vm/parity_tests.rs` via the module-graph runner, all on both engines; `examples/str_methods.chz` gained the multi-char/non-ASCII/negative-width/no-shrink golden lines. In-repo callers (`std/datetime.chz`, `examples/std_demo.chz` — all single-char fill, `width >= len`) are bit-identical. Docs: `docs/stdlib.md` (`pad_left` row + the alias carve-out). `str.pad_right`/`center` remain unimplemented (`docs/gaps.md`) — untouched.
- ✅ **`return` inside a `defer:` / `spawn:` block is now a check-time error (was: type-checked, then silently discarded at runtime)** (2026-07-13, `auto-task/reject-return-defer-spawn`) — `fn f() -> int!: defer: return Err("hijack")` / `spawn: return 7` checked OK (the return was even validated against the enclosing fn's return type) and then the value was dropped at runtime on both engines — the worst of both worlds. Chezzi has no named return values: a `defer:` block is its own closure and a spawned task outlives the frame, so such a `return` can never mean anything. **Fix (checker-only, zero runtime/codegen change → parity- and perf-neutral):** reuse the escaping-flow guard `recover:` already uses — renamed `recover_escaping_flow` → `escaping_flow` (`src/checker/mod.rs`) and called it at the `Defer(DeferTarget::Block)` + `SpawnTarget::Block` arms (`src/checker/sig.rs`) with `in_loop = true` (both arms already zero `loop_depth`, so `break`/`continue` keep their own "break outside loop" message — no double diagnostic). Messages: `'return' is not allowed inside a defer block` / `… spawn block` — one diagnostic, naming the block the `return` is LEXICALLY in: the walker does NOT descend into a nested `defer:`/`spawn:` block (each is guarded at its own site), so `defer: > parallel: > spawn: > return` reports "spawn block" once (it used to double-report, and `recover:` + `spawn: return` regressed the same way). The walker DOES descend into `wait:` arms + their `else` (a `return` there was the same silently-discarded bug; this tightens `recover:` identically). Still legal (boundary-tested): a `return` inside a nested `fn` DECLARED in either block, `return` in a `parallel:` body, `?`-in-`defer:` (still short-circuits + discards), and every defer/spawn/parallel nesting combination. (A closure body is a single expression, so it can't hold a `return` at all.) +14 checker tests (RED-first via `entry_rejects` / error-count assertions, incl. `wait:`-arm + `wait:`-`else` + nested-block-noun) + a two-engine `vm::defer_spawn_nesting_matrix_parity` regression net. Docs: `docs/syntax.md` (defer block form + spawn nursery; also fixed a stale `# prints "x = 1" — snapshotted here` comment → by-reference, reads `2` at exit).
- ✅ **L3-1: a pure-panic inline body (`fn boom(): panic("x")`) now infers `-> nil`** (2026-07-12, `auto-task/inline-panic-infers-nil`) — a function whose SOLE body is a diverging call was spuriously rejected with "cannot infer return type of '<name>'; add a -> annotation", even though a void body (`fn v(): print(...)`) infers `-> nil` and an annotated `fn b() -> int: panic(...)` type-checks (bottom fits any return). Root cause: `panic(...)` is bottom-typed `Ty::Unknown` (`expr.rs`), so the inline-expr body path (`src/checker/sig.rs:390`) set `inline_ret = Some(Unknown)`, which the finalizer rejected as residual un-inferable. Fix (checker-only, ~one branch): bind `let t = self.infer(e)` and substitute `Ty::Nil` iff `t.is_unknown() && Self::expr_is_diverging_call(e)` (the existing narrow bare-`panic`/`exit`/`os.exit` predicate) — so the caller can't use a value anyway, `nil` is the natural default. `self.infer(e)` still runs so panic's arg checks fire in pass 2. Genuine un-inferable cases (Err-only/None-only/`[]`/pure recursion — all block-body `return` forms) still rejected untouched. +2 tests RED-first: `checker::inline_diverging_body_infers_nil` (infers nil; `panic(123)` still rejected) + two-engine `vm::inline_panic_body_faults_both_engines` (caller prints "start", faults "x", exit 1; `recover:`→`Err("x")`). CLI-verified on `run` + `run --serial`. Docs: `docs/syntax.md`.
- ✅ **Deadlock fault message is now engine-agnostic + manifest entrypoint rejects `/` path separators** (2026-07-12, `auto-task/low-fixes-deadlock-entrypoint`) — two small independent fixes. (1) A blocking `recv`/`for v in ch:`/`wait:` with no producer and no runnable task deadlocks on BOTH engines, but the message hardcoded "sequential executor cannot block waiting for a producer" — misleading under the default M:N engine (one code path serves both). Reworded the two `self.err(...)` sites (`src/vm/netio.rs:934`, `:1109`) to engine-agnostic "deadlock — nothing is queued and no task can ever send" (recv keeps its C5 mid-flight hint). Detection/exit/`recover:` catchability unchanged (text-independent). (2) `entrypoint_file` (`src/main.rs`) split the module path only on `.`, so a manifest `entrypoint = "src/main:main"` (slash instead of dotted) resolved by accident via `PathBuf::push`; added a guard rejecting embedded `/`/`\\` with "the module path must use '.' separators, not '/'". Valid dotted forms unaffected. +3 tests RED-first (two-engine `deadlock_fault_message_is_engine_agnostic` + `deadlock_fault_is_recoverable_new_message` via `parity_entry_fault`; extended `entrypoint_file_validates_dotted_path`). Docs: `docs/cross-nursery-flat-scheduler.md`, `docs/concurrency.md` (paraphrased fault quotes updated). Text-only + guard-only; no engine-behavior change.
- ✅ **`i64::MIN` decimal literal `-9223372036854775808` now lexes/parses (was "number too large")** (2026-07-11, `auto-task/three-small-fixes`) — the boundary value i64::MIN was unwritable as a literal: the lexer read the positive magnitude `9223372036854775808` (== i64::MAX+1 == 2^63, which overflows i64) BEFORE the parser applied the unary minus, so it lex-errored "number too large to fit in target type"; users had to write `-9223372036854775807 - 1`. **Fix (lexer + parser, no checker/VM change):** the lexer's decimal-int branch (`src/lexer/mod.rs`), on an i64 parse overflow, now emits a distinct nullary `Token::IntMinMagnitude` iff the magnitude is EXACTLY 2^63 (via a `u64` reparse), else propagates the original error — so `9223372036854775809`+ still lex-errors. `parse_unary` (`src/parser/mod.rs`) folds `Neg` + `IntMinMagnitude` straight into `ExprKind::Int(i64::MIN)` (NOT `Unary(Neg,…)` — runtime-negating the magnitude would overflow); `expect_pattern_int` mirrors the fold for match/range patterns (`-9223372036854775808:` works). A BARE (un-negated) `9223372036854775808` still errors "number too large" (via new arms in `parse_primary`/`expect_int`), so the value can never leak as a positive int. `IntMinMagnitude` maps to the `INT` grammar terminal in `conformance.rs::symbol()` (no grammar.bnf drift — `-INT` already covered). Radix forms (`0x8000000000000000` etc.) are a SEPARATE lexer branch and still error at i64::MIN — deliberately out of this minimal scope (known v1 limit). +3 tests RED-first (`lexer::lexes_i64_min_magnitude`, `parser::i64_min_literal_folds_to_int_min`, two-engine `vm::parity_tests::i64_min_literal_runs_parity` covering print + arithmetic + `match`), CLI-verified `-9223372036854775808` prints correctly on `run` + `run --serial`, bare 2^63 and `-9223372036854775809` still error. Docs: `docs/syntax.md` (literal examples).
- ✅ **`json.parse` rejects non-finite/overflowing numbers at decode (Go `encoding/json` policy)** (2026-07-11, `auto-task/three-small-fixes`) — `json.parse("1e400")` returned `Ok(Json.Num(+inf))`, manufacturing a value its OWN `json.stringify` then FAULTED on ("cannot serialize non-finite float to JSON") — parse produced something that could not round-trip, violating the documented invariant that parse "never emits [a value] that Chezzi's own `parse` would reject." (`decode[int]("1e400")` already returned a clean `Err`; only the raw `Json` parse layer was the outlier.) **Fix (one guard, pure Chezzi, `std/json.chz` `parse_number`):** after `f := float(raw)`, `if f - f != 0.0: return Err("invalid number: value out of range")` (the same finite-check idiom `num_str` uses — `f - f` is `0.0` for finite `f`, `NaN` for `±inf`/`NaN`), matching Go's reject-at-decode. Underflow to `0.0` (`1e-400`) is finite and stays `Ok`. +1 two-engine parity test (`json_parse_rejects_non_finite_parity`: `1e400`/`-1e400`/`[1e400]`→Err, `1.5`/`123`/`1e-400`→Ok); the existing `json_as_int_out_of_range_parity`'s `1e400` probe flips `NONE`→`PARSEERR` (updated same commit). CLI-verified on `run` + `run --serial`. Docs: `docs/stdlib.md` json section (parse-rejects-overflow, symmetric with the stringify fault).
- ✅ **`str.split` empty-input invariant locked (regression guard)** (2026-07-11, `auto-task/three-small-fixes`) — investigated a reported `"".split(",") == []` bug; it does NOT exist. `"".split(",")` already correctly returns a one-element list holding `""` (length 1, `x[0] == ""`) via Rust's `str::split` at `src/vm/call.rs`, honoring `pieces == separators + 1` on both engines. The report misread the list's DEBUG RENDERING: a single empty string renders as `[]` because `""` prints as nothing (same as `",".split(",")` → `[, ]`). No production change; added a regression test (`str_empty_split_returns_single_empty_element_parity`) asserting length + element rather than rendering. Docs: `docs/stdlib.md` split entry now states the empty-input result explicitly.
- ✅ **`Self` usable in inherent struct/enum/newtype method signatures + bodies (checker-only)** (2026-07-11, `auto-task/self-and-compound-overload`) — `Self` resolved inside a `protocol` method sig but was rejected (`unknown type 'Self'`) in a plain `struct`/`enum`/`newtype` inherent method — an arbitrary asymmetry (Rust allows `Self` in inherent impls, not just traits). Fix: a new checker field `current_self_ty: Option<Ty>` set (via `mem::replace`) at the method-sig hoist sites (struct/enum/newtype in `setup.rs`) and at `infer_fn_ret`/`check_fn_body` entry from their `self_ty` arg, plus ONE `resolve_type` arm (`"Self" if self.current_self_ty.is_some()`) placed AFTER the type-param arm — so a PROTOCOL method's `Self` (already in `type_params` as `Ty::Param("Self")`, `current_self_ty` left `None`) keeps its existential-param binding UNCHANGED, and `Self` outside any method stays `unknown type 'Self'`. Resolves to the CONCRETE enclosing `Ty::Struct/Enum/NewType(key, [own type params])`, so `-> Self` enforces the real type (returning a different type is still a type error) and a generic `Box[T]`'s `-> Self` carries `[Param(T)]` matching `return self`. Newtype included (deliberate consistency: it routes through the same `infer_fn_ret`/`check_fn_body` funnel). Checker-only — runtime already dispatches these methods; both engines byte-identical. +7 checker tests (RED-first, `entry_ok`/`entry_rejects` module-graph path) + golden `examples/self_method.chz` (struct/enum/newtype/generic, two-engine parity). Docs: `docs/syntax.md` (Self in inherent methods).
- ✅ **Compound assignment honors struct/enum/newtype operator overloading (checker-only)** (2026-07-11, `auto-task/self-and-compound-overload`) — `x OP= v` is documented as exactly `x = x OP v`, but `check_assign_value` (`sig.rs`) whitelisted only str+str / non-widening numeric / three collection forms, so `a += V(10)` was rejected (`cannot apply += to V and V`) for a `struct V` with an `add` overload even though `a = a + V(10)` type-checked. Fix: the arith-compound arm now also computes `overload_ok = op_overload_result(target, val, proto).is_some_and(|res| assignable(target, &res))` — reusing the SAME `op_overload_result` the binary-operator checker (`infer_binary`) consults (`Add`/`Sub`/`Mul`/`Div`/`Mod` from the op) — and accepts when it holds. Because `op_overload_result` returns `Some` only for same-typed operands satisfying the operator protocol (or a same numeric-newtype auto-flow), a no-overload struct / `V += int` / `Box[int] += Box[str]` all stay rejected — no blanket compound-assign acceptance; existing str/numeric/collection forms unchanged. Runtime already lowers `a OP= b` through the same `Op::Add`/… opcodes as `a = a OP b`, so parity is automatic (verified by running both engines). +6 checker tests (RED-first) + golden `examples/compound_overload.chz` (struct `+=` == explicit form, numeric newtype `+=`, two-engine parity). Docs: `docs/syntax.md` (compound-assignment overload note).
- ✅ **Named-factory-import member resolution fixed (checker-only) (gap #4)** (2026-07-08, `auto-task/named-factory-import-member-resolution`) — a NAMED import of a factory *function* (not its return type) — `import make from lib; w := make()` — left the returned struct/enum/newtype's FIELDS and METHODS unresolvable (`type Widget has no method 'bump'`), even though the checker correctly TYPED the value (it rejected `x: int = w`). Member lookup was wrongly gated on whether the type NAME was imported into the current module (the per-module `self.structs`/`enum_methods`/`newtype_defs` tables are populated only by a whole-module OR named-TYPE import; a named-fn import injects nothing), so only `Match`/`Response`/`ProcResult` (globally seeded) resolved import-free. Broke the documented equivalence — `import lib; lib.make().bump()` and `import make, Widget from lib` both WORKED while `import make from lib` alone FAILED for an identically-typed value — and documented stdlib APIs (`import manual from std.cancel; manual().cancelled()`; `import min_heap from std.collections; min_heap().push(3)`). **Fix:** a LAZY, MISS-ONLY fallback keyed on the value's OWN module-scoped identity — on a local-table miss, resolve the shape from the OWNING module's `ModuleSig` by scanning `module_sigs` for the def whose `type_key(mid,name)` == the value's key (helpers `struct_shape`/`owning_struct_def`, `enum_methods_of`/`enum_type_params_of`/`owning_enum_def`, `newtype_methods_of`/`newtype_type_params_of`/`owning_newtype_def` in `src/checker/setup.rs`; swapped 6 member sites in `expr.rs`/`pattern.rs`/`sig.rs`). Purely additive: fires only on a miss (never shadows the globally-seeded seeds, zero cost on the local-hit path), reads `module_sigs` only — does NOT touch `struct_names`/`bare_types`, so **naming/constructing an un-imported type still errors** (`import make from lib; w := Widget(1)` → `unknown type 'Widget'`) and a same-named LOCAL type is unaffected (distinct keys, local table hits first). Transitive cross-module works (a `helper` returning a `cancel.Token`, `main` importing only `helper` → `.cancelled()` resolves; `cancel` is a transitive graph dep present before `main` is checked). Checker-only — no VM/runtime change (whole-module import already ran at runtime on both engines). +9 checker tests (RED-first, `check_graph`/multi-file graph) + 3 two-engine runtime parity tests; CLI-verified all three import forms print `11` and the stdlib APIs run on `run` + `run --serial`. Docs: `docs/spec.md` (member-access-is-import-free clarification).
- ✅ **`json.stringify` faults (Go-style) on non-finite floats instead of emitting invalid JSON (Finding C)** (2026-07-10, `auto-task/json-nonfinite-fault`) — `stringify` emitted a `Json.Num` holding `NaN`/`+inf`/`-inf` as the bare tokens `NaN`/`inf`/`-inf` (`num_str` in `std/json.chz` fell through to `str(f)`), which are (a) not valid JSON and (b) rejected by Chezzi's own `json.parse` — so `parse(stringify(x))` failed and the library silently produced malformed output with no fault. **Policy chosen: FAULT, Go `encoding/json`-style** (over JS `null`-emission / Python's non-standard tokens) — clean here because `stringify` returns bare `str` with no `Result` channel, and prelude `panic(msg)` raises a recoverable `RuntimeError` catchable under `recover:` with a byte-identical message on both engines. **Fix (pure Chezzi, one guard, `std/json.chz` `num_str`):** as the FIRST body line, `if f - f != 0.0: panic("cannot serialize non-finite float to JSON")` — `f - f` is `0.0` for every finite `f` and `NaN` (≠ 0.0) for `NaN`/`±inf`, so it fires iff non-finite, INDEPENDENTLY of the ±9e15 int-collapse range below (a large FINITE float like `1e300` is outside ±9e15 but must NOT fault — reusing the range check would be wrong). +2 two-engine parity tests (`json_stringify_non_finite_faults_parity` covering NaN/+inf/-inf via `recover:` + `e.message()`; `json_stringify_finite_roundtrip_unchanged_parity` regression guard proving `1e300`/whole-floats/negatives/strings/nested Arr+Obj stringify byte-identically and round-trip), RED-first. Auto-applies to all engines (pure-Chezzi, identical bytecode). CLI-verified the task repro faults identically on `run` + `run --serial`. Docs: `docs/stdlib.md` json section (non-finite policy, previously unspecified).
- ✅ **Two JSON number→int boundary bugs fixed at the shared f64→i64 seam (both total, consistent)** (2026-07-08, `auto-task/json-int-boundary`) — JSON stores every number as an f64 and both int-conversion paths mishandled the float→i64 boundary in opposite wrong directions. **Bug 1 (`json.decode[int]`)** SILENTLY SATURATED an out-of-range JSON integer: `decode[int]("1000000000000000000000000000000")` (10^30) returned `Ok(9223372036854775807)` (i64::MAX), `-1e30` → `Ok(i64::MIN)`, u64-max → saturated — Rust's `f as i64` cast saturates and the `D::Int` arm (`src/vm/call.rs`) range-checked nothing (only `fract`/`finite`). Fix: a range guard before the cast — `if f < i64::MIN as f64 || f > 9_223_372_036_854_775_808.0 { Err(out of range) }` — STRICT `> 2^63` upper bound so i64::MAX (which f64-rounds to exactly 2^63) still round-trips via the saturating cast while everything strictly beyond is rejected. **Bug 2 (`json.as_int -> Option[int]`)** FAULTED (uncaught, aborts the program) on out-of-range: `as_int` on `9999999999999999999` called the builtin `int()` which RAISES out-of-range. Fix (pure Chezzi, `std/json.chz`): gate on the same i64 window and `return None` when unrepresentable/non-finite, with a `n == 2^63` saturate special-case → `Some(i64::MAX)` (since `int(2^63)` itself faults). Both sites now map 2^63→i64::MAX, reject `|x|>2^63`, accept i64::MIN — for the identical out-of-range input `decode[int]` returns `Err` and `as_int` returns `None`; neither saturates, neither faults. Return type stays `Option[int]`; fractional truncation (`as_int(2.5)`→`Some(2)`) unchanged. +3 two-engine parity tests (`json_as_int_out_of_range_parity`, `json_decode_int_out_of_range_parity`, `json_int_boundary_consistency_parity`), RED-first, incl. the i64::MAX/MIN round-trip fences. Residual (inherent): a value exactly `2^63` (`"9223372036854775808"`) maps to i64::MAX at both sites — f64 cannot distinguish it from i64::MAX — consistent, not a saturation of the reported `|x|>2^63` class. Docs: `docs/stdlib.md` json section.

- ✅ **`print`'s `str(self)` display hook is gated on ACTUAL `Stringable` conformance, killing an uncatchable SIGABRT** (2026-07-04, Bug B) — the VM stringifier selected a user `str(self)` method as the implicit display hook by NAME + ARITY only, ignoring its return type. A `fn str(self) -> S` (returns the struct, not `str`) was chosen; the stringifier got an `S` back, re-stringified it, re-invoked the hook, and recursed forever → `fatal runtime error: stack overflow` (SIGABRT, uncatchable by `recover:`) on a check-accepted program. Fix (VM-only, `src/vm/stmt.rs` struct/enum/newtype display-hook arms): invoke the hook, then use its result ONLY when the RETURNED VALUE is a `str` (new `Vm::is_str_value`, mirroring `arith.rs` `struct_hash`/`enum_hash`'s invoke-then-check shape); a non-`str` result is NOT re-stringified — it falls back to the default repr, like a wrong-arity `str` already did. Checking the returned VALUE (not the declared syntax) covers an annotated `-> str`, an INFERRED (un-annotated) str, and a str type-ALIAS return alike — an earlier syntactic `-> str` gate was rejected in review because it silently regressed idiomatic un-annotated/aliased `str` hooks to the default repr (checker-vs-VM `Stringable` divergence). GC-safety: the non-`str` fallback re-reads the LIVE rooted struct/enum/newtype (the hook may have mutated a field and swept the pre-hook clone), so the default render never dereferences a dangling GcRef. `str` stays a normal user method — a direct `obj.str()` returning non-str is untouched (no checker rejection). +5 tests (`src/vm/tests.rs`, all `assert_mc_parity` → serial + M:N): the repro (struct/enum/newtype self-return → default repr), annotated/inferred/aliased str all still used by print + interpolation, direct-call-returns-non-str still works, and a GC-stress fallback (hook mutates a non-interned `List` field + 100k-alloc loop + returns self → correct re-read state, no panic). Docs: `docs/syntax.md` Stringable section now documents the display-hook resolution rule.
- ✅ **A runtime fault inside a `"{…}"` interpolation fragment now reports the fragment's real source LINE (was `line 1, col 1`)** (2026-07-04, `auto-task/interp-span-fix`) — any runtime fault (div-by-zero, index-out-of-bounds, integer overflow, …) whose faulting op sat INSIDE a string-interpolation fragment reported its span as `line 1, col 1` instead of the fragment's true line; the identical fault OUTSIDE interpolation reported correctly. This was the runtime/compiler counterpart of the 2026-06-30 `never-recover-span` checker fix (which corrected only the fragment ROOT nil-error span). Root cause: interpolation fragments are re-lexed from the escape-processed `raw` substring via `lexer::tokenize` (`src/interpolation.rs`), and `Lexer::new` hardcodes `line = 1`, so every fragment token span — and thus the arith/index opcode span the compiler emits and `Vm::err` renders — was fragment-relative (root at 1,1). Both serial and M:N VM share this codegen, so both printed the identical wrong span (why two-engine parity never caught it — it is a shared misleading-diagnostic bug, not a divergence). Fix (Strategy A, span-metadata only, runtime-inert): added a `base_line: usize` field to `Lexer` (default 0 → all normal lexing byte-identical) applied in the sole span funnel `span_at` as `line: self.line + self.base_line`, plus `Lexer::new_at` / free `lexer::tokenize_at`; `parse_interpolation` passes `base_line = span.line - 1` per fragment — the string literal's OPENING source line — so a fault inside any fragment reports that real line instead of `line 1`. We anchor to the opening line rather than the fragment's exact inner line ON PURPOSE: `raw` is the post-escape payload, where a `\n` ESCAPE and a genuine (triple-quoted) source newline are indistinguishable, so counting newlines in `raw` would inflate the reported line past an escape and point at UNRELATED code (a confidently-wrong diagnostic — flagged and fixed in review before merge). Opening-line is honest and never misattributes. COLUMN stays best-effort/fragment-relative (`col 1`) — also unrecoverable from the escape-processed substring. The shared parser hands the checker opening-line fragment spans too (symmetric, zero blast radius). +6 tests (`src/vm/tests.rs`): div-by-zero / index-OOB / overflow inside interpolation each report line 4 on BOTH engines, a multi-line triple-quoted fragment attributes to the string's OPENING line 4, a `\n`-escape-before-fragment fault stays on line 4 (regression guard against the escape miscount), and a non-interpolation fault + valid interpolation regression proves `base_line=0` leaves normal lexing byte-identical — all RED-first. Docs: refreshed the now-stale span doc comment in `src/checker/pattern.rs::check_interpolation`.
- ✅ **A nursery deadlock-abort now preserves a still-parked task's buffered stdout (two-engine parity)** (2026-07-05, `auto-task/deadlock-flush-parked-stdout`) — when a `parallel:` nursery was aborted by the M:N scheduler's deadlock detector, a still-PARKED task's ALREADY-buffered stdout was silently DISCARDED on the default M:N engine, while `--serial` printed it — a two-engine divergence on a DETERMINISTIC program (a consumer that prints three lines then blocks forever on a second `recv()` lost all three on `chezzi run`, kept them on `run --serial`). This was the exact gap the fault-output-flush entry below left open: `SchedCore::flag_deadlock` (`src/vm/mod.rs`) wrote each parked fiber's `TaskOutcome::Fault` slot with `out: String::new()`, discarding the fiber's own buffered output (`swap_ctx` had moved it into `f.ctx.out`/`f.ctx.stderr` when it parked), so the downstream `reduce_task_slots` propagated the deadlock error with an EMPTY buffer. Fix (~4 lines, the exact analogue of 888684d's real-fault fix): `flag_deadlock` now moves `f.ctx.out`/`f.ctx.stderr` into the `Fault` slot instead of allocating empties (`task_index`/`scope_id` are Copy, read before the partial move). `reduce_task_slots` was UNCHANGED at the time — it flushed only the lowest-index propagating fault's buffer at its task-order slot, so for a sole-printer parked task the transcript is byte-identical to serial; the MULTI-parked case was left open (now closed by the entry below). Scoped to the ONE `flag_deadlock` method (reached only by the M:N nursery deadlock detector); serial, real-fault, completed-sibling, and non-nursery-deadlock paths never route through it, so none regress. +1 two-engine parity test (`parallel_nursery_deadlock_flushes_parked_stdout_2engine`, sole-printer, looped 50× for interleaving flakiness), RED-first on the M:N arm only (serial passed pre-fix). Docs: `docs/concurrency-tier-d.md` (Decision F), `docs/concurrency-b3.md` (B3.5).
- ✅ **A MULTI-parked nursery deadlock-abort now flushes EVERY parked task's buffered stdout (two-engine parity)** (2026-07-05, `auto-task/multiparked-deadlock-flush`) — closes the remaining gap the entry above left open. When a `parallel:` nursery deadlocked with TWO-OR-MORE parked fibers, only the LOWEST-index propagating fault's buffer flushed on M:N, so a HIGHER-index parked fiber that printed before blocking had its output SILENTLY DROPPED — while `--serial` printed it live (a two-engine divergence on a DETERMINISTIC program: `silent`/idx0 parks empty and wins `first_fault`, so `printer`/idx1's `HI-FROM-PRINTER` was lost on `chezzi run`, kept on `run --serial`). Root cause: `flag_deadlock` faulted every parked fiber into a `TaskOutcome::Fault` slot, and `reduce_task_slots`'s `first_fault.is_none()` guard flushed only the first. Fix (3 touch points, additive): a DISTINCT `TaskOutcome::Deadlocked{err,out,stderr}` variant (`src/vm/mod.rs`), emitted by `flag_deadlock` instead of `Fault`; `reduce_task_slots` (`src/vm/sched.rs`) gets a `Deadlocked` arm that ALWAYS flushes out/stderr at its task-order slot (no `is_none()` gate) and a `deadlock_err` local, propagating ONE deadlock error (terminal match precedence Exit>Fault>Deadlock). Chosen over string-matching `DEADLOCK_MSG` (collision-fragile with a user `panic("deadlock: …")`) — a separate variant keeps the REAL-fault multi-fault ordering (the residual race the prior entry documented) provably byte-identical. `Deadlocked` never coexists with a real `Fault`/`Exit` (a real fault/exit trips `terminate` first). +2 two-engine parity tests: `parallel_nursery_deadlock_multiparked_flushes_higher_index_2engine` (2-fiber, sole-printer, order-deterministic exact match) and `parallel_nursery_deadlock_multiparked_multiprinter_set_3parked` (3-parked, two disjoint printers, order-insensitive SET via `assert_same_lines` per mn-parity discipline), both looped 50×; RED-first on the M:N arm only. Unit `mnsched_deadlock_when_all_parked_runq_empty` updated to assert `Deadlocked`. Verified end-to-end on the release binary at `--serial` and `--threads=1/2/8` (byte-identical `HI-FROM-PRINTER` on stdout, deadlock error on stderr, exit 1, thread-count independent). Docs: `docs/concurrency-tier-d.md` (Decision F), `docs/concurrency-b3.md` (B3.5).
- ✅ **A faulting `--parallel` task now preserves the stdout it buffered before the fault (two-engine parity)** (2026-07-04, `auto-task/fault-output-flush`) — a spawned task that FAULTS (panic / uncaught runtime error propagating to the nursery join) silently DROPPED all stdout it emitted *before* the fault, but ONLY on the default M:N OS-thread engine — the `--serial`/interp oracle preserved it (a two-engine divergence losing the user's debug output right before a crash, on the DEFAULT `chezzi run`). Root cause in `src/vm/mod.rs`: `TaskOutcome::Fault` was a tuple variant carrying NO buffered output, unlike `Exit { code, out, stderr }` and `Done(WorkerResult)`; every Fault construction site dropped the shell buffer and `reduce_task_slots` flushed only `Done`/`Exit` output in task order. The old rationale ("a faulting worker's partial output never had a deterministic position") was wrong for the LOWEST-index *propagating* fault: it has exactly the slot position the serial engine emits it at (after lower-index Done/Exit, before the propagated error is handled). Fix (localized to the fault-output-flush seam): made `Fault` a struct variant `{ err, out, stderr }` mirroring `Exit`; the real Fault sites that own a live buffer (`run_outcome`, `classify_mn_outcome`) `mem::take` the shell `out`/`stderr` (the Rust-panic-to-fault site carries an empty buffer; the deadlock-terminate site was later fixed to carry the parked fiber's own buffer — see the deadlock-flush entry above); `reduce_task_slots` flushes the terminal (`first_fault.is_none()`) fault's buffer INLINE at its task-order slot, then records the error. Higher-index racy faults + `Cancelled` still drop (no deterministic slot); `Exit`-over-`Fault` precedence byte-for-byte unchanged; the cooperative/`--serial` oracle untouched (the fix makes the default engine MATCH it). Fault-free goldens only ever hit `Done`, so byte-identical. **Residual (intentionally not chased):** byte-for-byte oracle parity holds only when the faulting task is the nursery's SOLE output-producer — with additional output-producing siblings the M:N transcript can still diverge from serial's stop-at-first-fault order (a sibling reaching `Done` before the faulter's cancel-trip keeps output serial never produced; `Fault`-vs-`Cancelled` classification is itself a scheduler race), a pre-existing nondeterminism the buffer-and-flush model cannot reconcile. +1 three-engine parity test (`parallel_faulting_task_flushes_partial_output_3engine`, single-faulter — the only deterministic fault-output shape), RED-first on the `--parallel` arm only. Docs: `docs/concurrency-tier-d.md` (Decision F), `docs/concurrency-b3.md`.
- ✅ **Float→string for large integral floats now shortest-round-trip-correct** (2026-07-01, `auto-task/float-shortest-roundtrip`) — the integral-valued branch of float formatting used `format!("{x:.1}")` (exact fixed-point expansion of the binary `f64`), so a large whole-valued float printed the artifact digits of its binary value instead of the documented shortest decimal that round-trips: `1.5e23` → `150000000000000004194304.0` (should be `150000000000000000000000.0`), `6.022e23` → `602200000000000027262976.0`. Contract (`docs/syntax.md:1787`, unchanged — the docs already promised "shortest-round-trip-correct … spelled out in full") was violated by the implementation. Fix: render the integral branch via Rust's default shortest `{}` Display (guaranteed fewest round-tripping digits AND never scientific notation for f64) then append a literal `.0` to preserve Chezzi's always-a-decimal-point invariant — `format!("{x:.1}")` → `format!("{x}.0")` in ALL THREE lockstep sites (`vm::format_float`, `interp::value::format_float`, `fmtspec::format_float_like`, single commit) so the stringify path and the bare-format-spec path stay identical and VM==interp holds. Behavior-preserving for every already-correct case (`3.0`, `-0.0`, `0.0`, `100.0`, `1e20`→`100000000000000000000.0`, negatives); the explicit `:e`/`:f`/precision spec arms are untouched. +1 golden `examples/float_large_integral.chz` (bare interpolation + bare-spec + small-integral controls, VM==interp==`.expected`), RED-first; `examples/literals.expected` avogadro artifact updated to the shortest form.
- ✅ **Never/bottom `recover:` payload now CONSISTENT + accurate interpolation-fragment nil-error span** (2026-06-30, `auto-task/never-recover-span`) — two coupled checker-only corner fixes. (A) `infer_recover` (`src/checker/mod.rs`) typed a `recover:` block by its tail: a direct `recover: panic(...)` went through the `StmtKind::Expr` arm and `infer`'d `panic` to bottom (`Ty::Unknown`, accepted as an `Ok` payload in value position), but a tail whose panic was reached through one more **statement-form** layer (`recover:\n  match 1:\n    _: panic("boom")`) went through `_ => check_stmt` and left `value_ty = Ty::Nil`, so the `Ok(v)` payload typed as `nil` and was rejected ("expression returns no value (nil) and cannot be used as a value") — the SAME bottom value usable in one path, banned in the other. Fix: after the tail check, `if value_ty == Ty::Nil && Self::stmt_terminates(last) { value_ty = Ty::Unknown; }`, reusing the existing sound, conservative divergence predicate (statement-form match all-arms-terminate, `while true:`, all-branch-returning `if/else`, trailing `exit`/`panic`). Both repro forms now accept; the `== Ty::Nil` guard keeps concrete-tail (`recover: 5` → `int`) and non-diverging-statement-tail (`recover: x := 5` → `Result[nil]`, still nil-banned) recovers untouched. (B) `check_interpolation` (`src/checker/mod.rs`) inferred each `{…}` fragment from a sub-parse with fragment-relative spans (root at `line 1, col 1`), so a nil-in-value-position error keyed on the fragment ROOT reported the bogus `(1,1)` fallback instead of the offending string literal; now stamps the whole-string-literal span onto the fragment root (`e.span = span`) before `infer_value`, matching the compiler's emit site, so the diagnostic carries the real line/col. Parity-safe by construction (check-time only; rejected programs never run, and the newly-accepted match-panic recover runs identically on VM and `--serial` → the panic takes the `Err` branch, the `Ok(v)`/`Unknown` arm is statically unreachable). +4 checker tests via `entry_ok`/`entry_rejects`/`check_entry` (diverging-match-tail accepts, direct-vs-match-panic consistency, concrete/non-diverging-tail regression fence, interpolation-void-fragment span ≠ (1,1)), all RED-first.
- ✅ **Expected-type inference: a type ANNOTATION now pins a generic ctor / generic fn-call's type params** (2026-06-30, `auto-task/expected-type-generic-inference`) — checker had checking-mode ("expected type flows IN") only for empty container literals and closures bound to `fn`-typed annotations; a generic constructor or generic function call was always inferred bottom-up, with the annotation used only as a POST-HOC `assignable_w` check. So `a: Heap[int] = Heap([], fn(x, y): x < y)` (and the return-type / call-arg forms) hit the un-inferable-closure-param deadlock — the empty `[]` couldn't pin `T`, the bare comparator params couldn't either, and `report_uninferable_closure_params` fired *before* the annotation could break the tie ("cannot infer type parameter `T` of `Heap`" + "cannot infer type of parameter 'x'/'y'"). Fix (checker-only, parity-safe by construction — generics are type-erased at runtime, no opcode/runtime change): a new `expected_hint: Option<Ty>` field threads the expected type from the three annotation sites — a `let`-binding's declared type (`StmtKind::Let` non-`ref` single-name non-closure branch), a function's declared **return** type (`check_return` non-closure branch), and a call **argument**'s declared parameter type (`infer_arg` non-closure branch) — into `infer_call`, which `take()`s it FIRST (so nested arg calls see `None`, no leak) and threads it through every generic ctor/call dispatcher (`infer_named_call` struct/newtype arms, `infer_qualified_struct_call`, `infer_newtype_call`, `infer_variant_call`, `infer_generic_call`). Each consumes it via a new `seed_from_hint(hint, &<declared-return-SHAPE>, &mut sub)` (`Struct(key,[Param…])` / `NewType` / `Enum` / a generic fn's `sig.ret`) placed AFTER arg-unification + `recover_iter_elems`/`recover_index_args` and BEFORE `report_uninferable_closure_params` — so precedence is **turbofish > arguments > annotation** (`unify` only binds a still-free param; an arg that pins `T` differently is the usual mismatch). Once `T` is seeded the existing `check_generic_arg` re-infers the comparator closure in checking-mode against `fn(int,int)->bool`, so the secondary "cannot infer parameter 'x'/'y'" errors also vanish. Bonus (same seam, for free): generic **newtype** ctor annotations (`e: Stack[str] = Stack([])` — previously needed a turbofish) and a return-only param of a generic fn (`xs: List[int] = empty()` for `fn empty[T]() -> List[T]`, previously "cannot assign List[T] to List[int]"). +6 graph_tests (3 primary repros let/return/call-arg + qualified-ctor + free-fn-return + a turbofish/annotated-closure/args-win-mismatch regression guard), all RED-first. **Remaining gap (documented, not forced):** a generic ctor nested inside a *container literal* (`a: List[Heap[int]] = [Heap([], …)]`) — the outer expr is a list literal, never reaches `infer_call`, so it would need a separate `infer_list` element-hint; annotate the closure params or turbofish there. Docs: `docs/syntax.md` (closure-param inference + generic-newtype §), `docs/spec.md` (new expected-type-inference note + newtype ctor), `docs/stdlib.md` (Heap §).
- ✅ **Container constructors `List[T]()` / `Map[K,V]()` / `Set[T]()` + bare `List()`/`Map()`; un-inferable-closure-param diagnostic; std standalone-check; std-module test wiring** (2026-06-29, `auto-task/container-ctor-turbofish`) — four bundled audit findings. **A (the ask):** the turbofish was rejected on every builtin but `Channel` — `name_is_generic` (`src/checker/mod.rs`) now also accepts `List`/`Map`/`Set`, and the three ctor arms read the type args (1 for List/Set, 2 for Map; arity-checked), so `List[int]()` pins an empty list's element type, `List[int]([1,2])` checks elements against it, and bare `List()`/`Map()` are now legal (mirroring the already-legal `Set()`), refined from the expected type / first use. **A is NOT checker-only** (audit's claim was wrong): 0-arg `List()`/`Map()` were rejected at RUNTIME in both engines too, so `builtin_list`/`builtin_map` in `src/vm/mod.rs` AND `src/interp/mod.rs` now return an empty container for 0 args (Set's existing shape) — two-engine parity held via a new `examples/container_ctor.chz` golden. **B:** `Heap([], fn(a,b): a<b)` leaked a misleading "cannot compare T and T" from inside the lambda; a new `report_uninferable_closure_params` guard (bare struct-ctor path + `infer_generic_call` + the module-qualified struct-ctor path `infer_qualified_struct_call`, so `c.Heap([], fn(a,b): a<b)` gets the same message) detects the genuine deadlock — an unbound type param appearing in an *unannotated* closure's PARAMETER slot — and emits "cannot infer type parameter `T` of `Heap`; annotate `Heap[T](…)` or the closure parameters", binding the param to `Unknown` to suppress the cascade. Scoped to parameter positions only (so `Mapped`-style `fn(T)->U` with `U` inferred from the body does NOT trip it) AND **probed against the closure body** (`trial_check_closure_args`, two trials — params left as the unbound `Ty::Param` vs. bound to `Unknown`): it fires ONLY when the body actually constrains the param (`a < b` errors unbound but is clean under `Unknown`), so a harmless body that doesn't need `T` (`each([], fn(x): print(x))`, `mapper([], fn(x): 42)`) keeps type-checking — it ran on `main` and must not be newly rejected — and an unrelated body error (errors under BOTH trials) is left for the normal per-arg check to report as itself. **C:** standalone `chezzi check std/…chz` reported phantom "unknown type 'RwShared'/'Shared'" — stdlib auto-privilege was granted only on the import path; `LoadedModule::is_std` (`src/resolver/mod.rs`) is now path-aware (file under `std_root()`), fixing the editor/LSP false positives; new lib test standalone-checks every `std/**/*.chz`. **D:** the committed per-module std test files (`collections`/`concurrent_collection`/`datetime`/`path`) existed but only 4 unrelated `_test.chz` were in the `cargo test` dogfood guard — all four are now registered (`src/test_runner.rs`). Docs: `docs/syntax.md` + `docs/stdlib.md` (turbofish + bare-empty constructor forms). `Shared`/`RwShared`/`Atomic` turbofish left OUT (value-first; tests intentionally reject it). **[SUPERSEDED 2026-06-30:** the value-first concurrency boxes now ALSO accept an optional, value-checked turbofish — see the "Turbofish construction on the value-first concurrency boxes" entry in *Current focus*.**]** Checker/resolver-only on the type side; both engines exercised; conformance green.
- ✅ **Negative int literal/range patterns + match-doc qualifier fix** (2026-06-28, `auto-task/neg-match-patterns`) — two bundled match-pattern changes, one commit. PART 1 (parser/grammar only — AST/checker/compiler/vm/interp already `i64`-signed, NO runtime change): a leading `-` in a pattern was a hard parse error ("expected identifier, found '-'"). Root cause: `parse_pattern_primary` (`src/parser/mod.rs`) only entered its int branch on `Token::Int`. Fix: new `expect_pattern_int` helper (eats an optional `Token::Minus` then `expect_int`), and widened the literal arm to `Token::Int(_) | Token::Minus`, using it for the literal AND both range bounds → `-3:`, `-10..-5:`, `-10..5:`, `0..-5:` all parse (and compose with guards/or-patterns). Stays **int-only**: a negative float `-3.0:` now routes through `expect_int` and is rejected "expected integer, found float" (no float pattern added; positive `3.0:` unchanged). A negative literal arm is still refutable — `_` is still required for exhaustiveness. `docs/grammar.bnf` `<patternPrimary>`: +4 `MINUS` alternatives + int-only comment; new accept/reject conformance corpus (`tests/corpus/accept/match_neg.chz`, `tests/corpus/reject/match_neg_float.chz`). +2 parser tests, +2 VM `run_parity` tests (neg literal/range + neg-with-guard/or), +2 checker exhaustiveness tests. PART 2 (doc-only): `docs/syntax.md` match/enum examples showed BARE arms (`Circle(r):`, `Leaf:`) but the impl requires QUALIFIED `Enum.Variant` — qualified all of them (Shape/Tree/Color groups) to match the implementation + prose; each edited snippet CLI-`check`ed clean. i64::MIN (`-9223372036854775808`) stays unparseable (lexer rejects the magnitude) — known limit, unchanged. Two-engine parity (VM==interp) green; `cargo test conformance` green.
- ✅ **`?` sum-type KIND soundness: a Result-`?` is rejected in an Option-returning fn (and vice versa)** (2026-06-27, `auto-task/try-kind-check`) — checker hole in `infer_try` (`src/checker/mod.rs`): the pre-computed `ret_err` collapsed both `Ty::Option(_)` and `Ty::Nil` enclosing returns to `None`, so (1) the `Ty::Result` operand arm SKIPPED its compatibility check whenever `ret_err==None` (a `Result`-`?` slipped through an `Option`-returning fn) and (2) the `Ty::Option` operand arm never inspected `current_ret` (an `Option`-`?` slipped through a `Result`-returning fn). The mistyped fn then returned the wrong sum-type and FAULTED a downstream exhaustive `match`/`??` at runtime ("no match arm for variant 'Err'/'None'") even though `check` passed. Fix (checker-only, no runtime/parity change — both engines inherit the stricter validation): dropped `ret_err`, folded a `current_ret` KIND match into each operand arm — `Result`-operand ⇒ enclosing must be `Result` (keeps the existing error-TYPE check) or `Nil`; `Option`-operand ⇒ enclosing must be `Option` or `Nil`; mismatched kinds get distinct errors ("'?' propagates a Result error, but the enclosing function returns Option, not Result" / "…returns Result, not Option"). `Nil` (top-level/`main`) still accepts either; `Unknown` enclosing (inferred-closure return) stays REJECTED. +4 checker tests (2 RED-first KIND-mismatch repros + 2 compatible-still-ok guards); existing error-TYPE + closure + recover guards stay green. Docs: `docs/syntax.md` §9 `?`-kind clarification.
- ✅ **Inferred struct/enum method return types now FLOW (soundness; closed an unchecked-struct-body hole too)** (2026-06-27, `auto-task/method-return-inference`) — checker bare-key vs module-key divergence: in the `build_graph`/`check_graph` path the struct layout is stored under `<module-key>::Name`, but `struct_self_ty` + `infer_returns_pass`'s struct branch + the pass-2 struct body-check guard all looked up `self.structs.get(name)` by the BARE name → misses. Three coupled defects, one root: (1) an un-annotated method's inferred return was written to a non-existent slot, so `s: str = P(3).val()` silently accepted `int` into `str` (and protocol satisfaction read `Unknown` — an inferred `compare→bool` wrongly satisfied `Comparable`); (2) `struct_self_ty` built `Ty::Struct(BARE, [])` corrupting `self`'s type; (3) the pass-2 guard missed → struct method **bodies were entirely UNCHECKED** in the entry path (`y: str = self.x` passed). Plus (4) `infer_returns_pass`/`count_uninferred` had no `Enum` arm → enum method returns never inferred. Fix (checker-only, parity-safe by construction — no opcode/runtime change): bare_key the three struct lookups + the `Ty::Struct` key (mirror the already-correct `enum_self_ty`), and add the `Enum` arm. Turning struct-body checking ON surfaced a pre-existing latent checker bug — the duplicate-binder pre-pass (`first_duplicate_binder`) counted a bare nullary-variant ident (`None`) as a binder, so `(None, None, None)` was falsely "bound more than once" (only ever hit inside struct method bodies, e.g. `examples/slicing.chz`); fixed by passing it the same variant-name predicate `bind_subpattern` uses. +11 checker tests (entry_rejects/entry_ok on build_graph path — the single-module `ok()`/`rejects()` helpers mask the bug). Newtype method-return inference left intentionally unfixed (out of scope; `newtype_self_ty` already key-correct) — known consistency follow-up. Docs: `docs/syntax.md` return-inference note (methods + protocol satisfaction).
- ✅ **Match-exhaustiveness soundness: guarded / refutable-payload variant arms no longer close a variant** (2026-06-27, `auto-task/match-exhaustiveness-guard`) — checker hole: `bind_match_arm` inserted a variant into the `covered` set UNCONDITIONALLY, ignoring (a) the arm's guard and (b) refutable payload sub-patterns (literals/ranges/nested-variants). So `E.A if false: …` / `Some(0): …` / `P.Pair(0, y): …` passed `chezzi check` then FAULTED at runtime ("no match arm for variant …"). Fix (checker-only, no runtime/parity change — both engines already fault identically): threaded `guarded: bool` into `bind_match_arm`, collected `payload_irref` from the existing `bind_subpattern` zip loop, and gated coverage on `!guarded && payload_irref`; duplicate-arm detection now keys on `covered.contains` (a PRIOR fully-closing arm) so the standard `E.A(n) if c` → `E.A(n)` guard-then-fallback idiom is ACCEPTED instead of wrongly rejected as "duplicate match arm". Tuple + int/str/bool literal scrutinees untouched (already conservative). +5 checker tests (4 RED-first repros + a duplicate-still-rejected regression guard). Docs: `docs/syntax.md` §8 refutable-payload clarification.
- ✅ **str methods (split-brain, minimal subset) + safe numeric parse** (2026-06-23,
  `auto-task/str-methods-safe-parse`) — gaps #1 (str half) + #7. Added 11 receiver methods on `str`
  that forward to the existing `std.str` free fns (`ends_with`/`replace`/`repeat`/`reverse`/`pad_left`/
  `index_of`/`count`/`strip_prefix`/`strip_suffix`/`split_lines` + `strip`, a `trim` alias) so
  `s.ends_with(x)` works like `s.starts_with(x)` with no import; plus `to_int() -> int?` /
  `to_float() -> float?` that return `Some`/`None` instead of raising on bad input (trim + `parse`,
  reusing the `int()`/`float()` parse path). Pure-native Rust in **both** engines (checker
  `str_method_sig`, VM `core_method` Str arm, interp `str_method`), byte-identical to the std.str
  codepoint-loop oracle — `index_of` returns a **codepoint** index (not Rust's byte offset), `replace`/
  `count` guard the empty-arg edge, `repeat` n≤0 → `""`. The `std.str` free fns are untouched
  (`examples/str_more.chz` still green). Golden `examples/str_methods.chz` exercises every method incl.
  multibyte + `Some`/`None`, asserted byte-identical across all three engines. Out of scope (left open):
  the full `std.iter`/`std.cmp` receiver re-export half of #1. Docs: `docs/stdlib.md` str method table +
  `std.str` note, `docs/syntax.md` method cheat-sheet, `gaps.md` (#1 str half + #7 → resolved log).
- ✅ **Left-shift overflow now a recoverable fault** (2026-06-23, `auto-task/shift-overflow`) — `1 << 63`
  silently wrapped to `i64::MIN`, violating the "every i64 overflow is a recoverable fault" policy
  (the shift handler validated only the shift-*amount* range, never value overflow, unlike `+ - * / %`).
  Fix (both engines, `vm/mod.rs` `bitwise()` + `interp/mod.rs` `eval_binary` Shl arm): a left-shift-only
  round-trip check — `(a << b) >> b != a` ⇒ raise the shared `integer overflow in Shl`. Round-trip-safe
  shifts incl. `-1 << 63 == INT_MIN` still succeed; `>>` is unchanged (arithmetic, never overflows).
  Golden `examples/edge_cases.chz` `shift_ovf63` probe pins it on all three engines + a VM unit test
  guards the non-overflow regressions. Docs: `gaps.md` nit resolved, `docs/spec.md` overflow policy +
  `docs/syntax.md` shift note updated.
- ✅ **`list.map`/`.filter`/`.fold` OOB-on-shrink fixed** (2026-06-21, `auto-task/list-hof-shrink-oob`) —
  VM `list_hof` captured `n = v.len()` once then indexed the *live* heap list, so a callback that
  shrank the receiver (`xs.pop()`) ran a stale index past the now-shorter `Vec` → `index out of bounds`
  panic (vm/mod.rs:6840 map/filter, ~6890 fold) on both engines. Fix: allocate a **rooted snapshot**
  of the receiver's elements at call time and index that (mirrors `list_sort_by`; the interp already
  snapshots `elems` before dispatch, so this aligns the VM to interp). **Chosen semantics: snapshot** —
  map/filter/fold iterate the receiver's elements as of call time; a callback that shrinks **or** grows
  the receiver does not perturb iteration (consistent with comprehensions/`for`-loops/Python). Tests:
  `map`/`filter`/`fold`_shrinking_callback_no_panic + golden `examples/list_hof_shrink.chz` (VM==interp).
  Docs: `docs/stdlib.md` (snapshot note), `gaps.md` (entry → ✅ RESOLVED).

- ✅ **User-callable `panic(msg: str)` builtin** (2026-06-20, `auto-task/panic-builtin`) — exposes a
  user-facing way to raise the **same** recoverable `RuntimeError` the runtime already uses internally
  (overflow / OOB / bad decode); the M11 `recover:`/`defer` machinery catches it unchanged. `panic`
  **unwinds** (it is NOT sugar for `return Err(...)` — that already exists for *expected* errors):
  caught by the nearest `recover:` as `Err(e)` with `e.message() == msg`, else it aborts the program
  with that message + non-zero exit (byte-identical to an integer overflow), running `defer`s on the
  way out. It is **bottom-typed** (`Ty::Unknown`, no new `Ty::Never`): type-checks as a statement, as
  a diverging branch tail (no explicit `return` — `expr_is_diverging_call` generalizes the `exit`
  precedent), and in value position (`x := if ok: v else: panic("no")` takes `v`'s type via
  `unify_branch`). Pure-builtin path — compiles to `Op::CallBuiltin("panic", 1)`; each engine's
  name-keyed dispatcher returns `Err(RuntimeError{message, span})` (VM `do_builtin` early-return /
  interp `eval_call` interceptor) instead of an `Ok` value. Registered across all four name tables
  (checker `is_reserved_name` + `builtin_call`, interp + compiler `is_builtin`). No grammar change
  (plain call). New golden `examples/panic.chz`; checker/interp/VM unit tests + cross-engine parity.
- ✅ **Match arms accept module-qualified enum-variant patterns (`geo.Color.Red`)**
  (2026-06-20, `auto-task/qualified-variant-patterns`) — match is now symmetric with construction:
  for an enum from a whole-module `import geo` you can write `match c:\n  geo.Color.Red:` directly
  (was a `parse error: expected ':', found '.'`; workaround was `import Color from geo` + bare
  `Color.Red`). The 3-part spelling is `module.Enum.Variant` (the binder is the bound module name —
  last path segment or `as` alias); `import geo as g` → `g.Color.Red:`; payload bindings work
  (`geo.Shape.Circle(r):`). A new `module_name: Option<String>` on `Pattern::Variant` carries the
  binder; the **parser** accepts an optional leading `IDENT.` (a 3rd dot deterministically means
  module-qualified — unambiguous); the **checker** (`check_pattern_qualifier`) validates the module is
  bound + owns the enum (errors render BARE names, never the `::` identity key) then resolves the enum's
  identity key and delegates to the existing scrutinee-driven validation; **both engines drop the binder**
  and key on the same `(enum, variant)` identity as the bare/named-import form, so VM == interp ==
  `--serial` == `--parallel` byte-for-byte (exhaustiveness unchanged, by identity). A bare user-variant
  is still rejected with the "write it qualified" hint; `Ok/Err/Some/None` stay bare; a 2-part
  `module.Variant` (dropping the enum) is NOT accepted. Docs: `docs/grammar.bnf` (+conformance green),
  `docs/syntax.md` match section.
- ✅ **C-ABI FFI: module-qualified type at the extern boundary (`mod.Type` / `mod.Alias`)**
  (2026-06-20, `auto-task/ffi-qualified-type`) — fixed a scoping bug in the module-scoped-types
  feature: a module-qualified type written at an `extern` boundary (`cdefs.DivT`, `w3.Len`, AST
  `Type::Qualified`) was not lowered to a C type, so the checker (which resolves `Qualified`) and the
  backends disagreed. Symptoms: a qualified RETURN struct silently became void (`cannot read field … of
  nil`); a qualified PARAM panicked the VM at the marshal loop's `.expect`. Root cause: `qualify_ffi_type`
  (compiler) and the interp `qualify` closure only rewrote a bare `Type::Named` struct → identity key and
  passed `Type::Qualified` through unchanged, so the byte-identical `ctype_of` twin (no `Qualified` arm)
  lowered it to `None`. Fix: both rewrites now resolve `Qualified { module: binder, name, .. }` via
  `imported_modules`/`module_types`/`type_keys` → a qualified STRUCT becomes `Named(identity_key)` (hits
  the identity-keyed `struct_fields`), a qualified WIDTH ALIAS becomes `Named(bare name)` (hits the
  bare-keyed `aliases`), all BEFORE `ctype_of` so the twin stays byte-identical. Also converted the
  param-marshal `.expect("checker verified marshallable param")` (both engines) into a graceful
  compile/runtime error mirroring the checker's "not C-marshallable" wording — a user program can no
  longer panic the VM via this path (the checker remains the real gate). Named-import spelling
  (`import DivT from core.cdefs`) already worked; only the DOTTED spelling was broken. Tests: three new VM
  parity tests (qualified return struct → 3/2, qualified width param → 7, non-marshallable qualified →
  clean error not panic), two new checker guard tests; full suite (2279) + conformance green, clippy
  clean. Docs: `syntax.md` §12b, `ffi-and-packaging.md`, this file. Out of scope (untouched): the
  separate "type alias to an FFI STRUCT at the boundary" inconsistency.
- ✅ **C-ABI FFI follow-up: module-qualified WIDTH ALIAS resolves to its DEFINING module's width**
  (2026-06-20, `auto-task/ffi-qualified-type-fix`) — the adversarial panel found the prior fix
  reintroduced the bare-name class for the WIDTH-ALIAS case: the qualified arm rewrote `mod.Alias` to a
  bare `Named(name)`, which `ctype_of` then resolved through the flat, program-global, **bare-keyed**
  `aliases` table (last-write-wins). So when two reachable modules both declared `type Len` with
  DIFFERENT widths (`core/w3.chz` int64 + a colliding local `type Len = int8`), `w3.Len` collapsed to
  bare `Len` and silently marshalled through the WRONG width — the checker said OK (int64) but all three
  engines printed `44` (int8-truncated `abs(-300)`) instead of `300`. Fix (module-scoped, mirrors
  `type_keys`): added a `module_aliases: (module_idx, name) → body` map to BOTH engines, populated
  alongside the existing alias gather; the qualified width-alias arm now looks up the body by the
  ALREADY-resolved defining-module index `tidx` and returns THAT (an `int64` width scalar `ctype_of`
  resolves directly, no flat-map hop), so a colliding local alias can't hijack the C ABI — matching the
  checker, which resolves a `Type::Qualified` alias via the defining module's `type_aliases`. The
  qualified STRUCT path, the non-colliding qualified width path, the bare/named-import path, and the flat
  `aliases` table are all untouched. Tests: one new VM 3-engine collision parity test (`w3.Len`=int64 +
  local `Len`=int8 → `abs(-300)`=300 on VM/`--serial`/`--parallel`); the existing non-colliding twin
  (→7), struct (→3/2), and clean-error guards stay green; full suite + conformance green, clippy clean.
  Docs: `ffi-and-packaging.md`, this file. (The single-hop fix's chained-alias gap is closed by the
  ROOT fix below — chains are now resolved fully module-scoped at all depths.)
- ✅ **C-ABI FFI FINAL ROOT fix: qualified/imported/aliased extern types resolve via the CHECKER**
  (2026-06-20, `auto-task/ffi-qualified-type-fix4`) — ended the AST-recursive alias-spelling
  whack-a-mole (fix..fix3 each closed one spelling and the next re-entered a flat bare-name alias map).
  Confirmed-still-broken on fix2: a **named-import chain hop** (`core/widths` = `import int64 from
  std.ffi` + `type W = int64`; `core/w3` = `import W from core.widths` + `type Len = W`; `main` =
  `import core.w3` + colliding `type W = int8` + `extern fn abs(n: w3.Len) -> w3.Len`) — `check` OK
  (w3.Len → W(from widths) → int64) but `run`/`--serial`/`--parallel` all printed **44** (main's
  colliding int8) instead of **300**. Root cause: the backend's `qualify_ffi_type`/`resolve_qualified_
  alias` only knew aliases DECLARED in the defining module (`module_aliases`); a name brought in via
  `import X from other` matched neither key and fell back to the flat last-write-wins bare `aliases`
  map → collision. **The robust fix (mandated): one resolver — the checker.** New
  `checker::resolve_extern_signatures(graph) -> ExternTable` runs the SAME deps-first module pass and,
  for each `extern` fn, records the fully-resolved width-bearing `CType` per param/return via a new
  `resolve_ctype` walk that mirrors `resolve_ty_ro`'s alias/`from`-import/`Qualified`/cycle logic but
  stops at the WIDTH leaf (`Ty` collapses every FFI width to `Ty::Int`, so the carrier must be a
  `CType`, not a `Ty`). The width crosses module boundaries via a new `AliasSig.ctype` (computed in the
  defining scope) + a parallel `imported_alias_ctypes` populated in `bind_import`. **Both backends now
  consume the table** (keyed by `(graph module idx, fn name)`, the index both derive) and NEVER
  re-resolve alias names — closing every spelling at once: single-hop, local chain (any depth),
  named-import hop, qualified hop, AND mixed chains. **Deleted** the dead machinery: `qualify_ffi_type`
  + `resolve_qualified_alias` + `module_aliases` in BOTH engines. (At fix4 the standalone source-string
  test path still kept a LOCAL-only `ctype_of` fallback — **that second resolver was deleted in fix5
  below**; the standalone path now goes through the checker too, so there is exactly ONE resolver.) The
  fix2 "cross-module qualified body mid-chain (`type Len = other.X`)" `None`
  case is now resolved too (the checker has each module's real import-binder map). Tests: new VM 3-engine
  parity tests for the named-import hop and a LOCAL→named-import→QUALIFIED **mixed** chain (each hop a
  collision, all → 300 on VM/`--serial`/`--parallel`), 7 new checker `resolve_ctype` unit tests
  asserting the exact `CType` per spelling (the dual-resolver-drift guard), and all prior FFI guards
  (single-hop/chain collisions → 300, struct → 3/2, width param → 7, cyclic → clean error, non-
  marshallable → clean check error) stay green. The stale `extern_cross_module_alias_runs` test (which
  asserted a BARE cross-module alias the checker now rejects as module-scoped) was corrected to the
  `import Size from sizes` spelling. Full suite (2292) + conformance green, clippy `--all-targets`
  clean; CLI repro 20×/`--parallel` deterministic at 300.
- ✅ **C-ABI FFI ARCHITECTURALLY-FINAL fix: struct FIELDS resolve in the STRUCT's defining scope +
  the second resolver is DELETED** (2026-06-20, `auto-task/ffi-qualified-type-fix5`) — closed the one
  regression the fix4 redesign introduced and made dual-resolver drift structurally impossible. **The
  regression:** a qualified/imported extern RETURN STRUCT whose FIELDS are typed via the DEFINING
  module's local alias (`core/cdefs.chz`: `type Half = int32` + `struct DivT{quot:Half; rem:Half}`;
  `main`: `extern fn div(...) -> cdefs.DivT`) resolved to a **void return (nil)** — `run`/`--serial`/
  `--parallel` all faulted with `cannot read field 'quot' of nil` (expected quot 3, rem 2). Root cause:
  the checker's `resolve_struct_ctype` read the struct's raw field ASTs but resolved each field via
  `resolve_ctype_d`'s alias arms against the **importing** module's `aliases`/`imported_alias_ctypes`,
  where `Half` is invisible → field `None` → whole-struct `CType` `None` → backend lowered the return as
  void. **Structural fix (extends the `AliasSig.ctype` precedent to structs):** a graph-wide
  `struct_ctypes: HashMap<identity-key, Option<CType>>` cache on the `Checker`, populated once per module
  after `hoist` (all that module's aliases/`from`-imports live) and before the check_stmt loop, each
  struct's complete by-value `CType::Struct` computed **in its OWN defining module's scope**. Modules are
  checked deps-first, so an importer's extern returning `mod.Struct` reads the cached defining-scope CType
  **verbatim**; `resolve_struct_ctype` became a pure cache read (the bare/same-module arm keeps a
  field-walk fallback in the defining scope for forward-ref nested structs; the qualified arm NEVER
  field-walks — it only reads the cache). **Single-resolver enforcement (deletion):** removed the
  backends' second resolver entirely — `compiler::ctype_of`/`ctype_of_visiting` + `gather_aliases` + the
  `aliases` field + their `ctype_of_maps_*`/`ctype_of_struct_cyclic_alias_no_overflow` tests, and
  `interp::ctype_of`/`ctype_of_visiting` + the `extern_aliases`/`extern_struct_fields` fields + their
  gather loops + parity-twin tests. The two `.or_else(ctype_of…)`/`None => ctype_of(…)` fallback arms are
  gone; both backends now read `extern_sigs` (the checker's `ExternTable`) **verbatim**. The standalone
  single-file paths (`compile_module_standalone`, `Interp::execute`) route through a new
  `checker::resolve_extern_signatures_standalone(stmts)` (a synthetic one-module `<main>` graph
  delegating to the same `resolve_extern_signatures`), so there is now **exactly ONE** extern-type
  resolver in the codebase — drift is impossible by construction. (`compiler::struct_fields` is retained
  for `json.decode` only; it no longer feeds extern lowering.) Tests: new checker
  `resolve_extern_ctype` units (aliased-field regression repro; a named-import + qualified + nested
  struct-field case where each field's DEFINING width wins over a colliding importer alias), a VM
  3-engine `extern_qualified_return_struct_aliased_field_runs` (quot 3 / rem 2 on VM/`--serial`/
  `--parallel`), and a standalone-path `extern_standalone_source_string_struct_return_runs` guard locking
  the single-resolver wiring; all prior FFI guards (single-hop/chain/named-import/mixed → 300, plain
  struct → 3/2, width param → 7, cyclic → clean error, non-marshallable → clean check error) stay green.
  Full suite (2290) + conformance green, clippy `--all-targets` clean; CLI struct-aliased-field repro
  20×/`--parallel` deterministic at 3/2.
- ✅ **C-ABI FFI ROOT fix: module-qualified width-alias CHAIN resolves module-scoped at ALL depths**
  (2026-06-20, `auto-task/ffi-qualified-type-fix2`; **superseded by fix4 above** — the backend
  re-resolvers it added are now deleted) — the deeper adversarial find on the single-hop
  fix above: it only resolved the FIRST hop in the defining module's scope. A CHAINED qualified alias
  (`type Len = Inner; type Inner = int64` in `core/w3`) returned w3's RAW ONE-HOP body (`Named("Inner")`)
  and handed it to `ctype_of`, which resolved the INNER name `Inner` through the flat, last-write-wins,
  **bare-keyed** `aliases` map — so a colliding `type Inner = int8` in the CALLING module hijacked the
  inner hop. `check` was correct (the checker fully resolves the chain in the defining module's scope),
  but `run`/`--serial`/`--parallel` all printed `44` instead of `300`; the same fault held at depth 3+.
  Fix: a new `resolve_qualified_alias(tidx, name, …)` helper in BOTH engines follows the WHOLE chain
  in its defining module's scope (each inner bare `Named(inner)` is interpreted as `tidx`'s `inner` via
  `module_aliases`/`type_keys`), so NO hop ever re-enters the flat bare `aliases` map; it returns a
  scalar/FFI-width LEAF or a struct identity key, never a re-entrant alias name. The qualified-alias arm
  in `qualify_ffi_type` (compiler) / the `qualify` closure (interp) now calls it. Bounded by a visited
  `(module_idx, name)` set: a cyclic alias (`type A = B; type B = A`) ⇒ `None` ⇒ `ctype_of`'s clean
  "not C-marshallable" error — no hang, no stack overflow, never a silent wrong width. A cross-module
  qualified body mid-chain (`type Len = other.X` declared inside the defining module) is the one
  remaining `None` case (it needs that module's own import-binder map, not threaded here) — a clean
  error, not the bare-`Named`-chain family this closes. Both engines kept byte-identical in logic
  (two-engine parity). Tests: new VM 3-engine parity tests at depth 2 AND depth 3 with colliding inner
  alias names across modules (`abs(-300)`=300 on VM/`--serial`/`--parallel`) plus a cyclic-alias
  clean-error/no-hang test; the single-hop collision (→300), non-colliding width (→7), struct (→3/2),
  and clean-error guards stay green; full suite (2283) + conformance green, clippy `--all-targets`
  clean. Docs: `ffi-and-packaging.md`, this file.
- ✅ **C-ABI FFI follow-ups: `bool`=C `_Bool`, precise width-alias gate, redundant self-rename allowed**
  (2026-06-18, `auto-task/ffi-bool-cbool-alias-gate`) — three FFI loose ends from the prior reviews.
  (1) **`bool` now means C `_Bool` (1 byte)**, not C `int` (4 bytes): re-mapped `CType::Bool`'s libffi
  lowering in `src/native/cffi.rs` only — `ffi_type` → `Type::u8()`, param `Vec<u8>`, `write_field`/
  `read_field` 1 byte, and a `_Bool` **return reads register-width then narrows to a byte + `!= 0`** (the
  libffi rvalue-widening rule, same as the narrow-int OOB fix). `ctype_of` is unchanged in **both**
  engines (the divergence hazard doesn't apply; both call the shared `Cffi::call`), so parity holds. A
  struct `_Bool` field now has correct 1-byte size/offset — closing the prior footgun. **Behavior change:**
  a C function using the int-as-bool idiom (`isdigit`, arbitrary nonzero `int` for true) must be bound
  `-> int` and tested `!= 0`, **not** `bool`. There is **no separate `bool8` type** (the planned one is
  mooted). (2) **Closed the width-alias gate hole** (`!alias_resolving.is_empty()` relaxation in
  `resolve_type`): a `type Len = int32` whose defining module never imported `int32` no longer launders the
  bare width name. The opt-in is now **precise** — recorded in a program-global `ffi_alias_ok` set at
  alias-definition time (only when the defining module imported the width); the gate accepts a width name
  through an alias iff the innermost resolving alias is licensed. (3) **Allow the redundant identical
  self-rename** `import int32 as int32` (was rejected "cannot be renamed"): the guard now fires only when
  the as-name differs from the member — a true rename (`as W`) or wrong-width trap (`int8 as int32`) still
  rejects. Tests: `cffi.rs` `bool_marshals_as_one_byte_cbool` + `struct_bool_field_marshals_one_byte`;
  `checker/tests.rs` `width_alias_without_any_import_rejected` + `width_alias_defined_with_import_resolves_in_extern`
  + `width_import_redundant_self_rename_ok` (all RED-first). Docs: `syntax.md` §12b, `spec.md` §Level-3,
  `ffi-and-packaging.md` §1b (supersedes the `bool8` note). Two-engine parity green on the FFI examples.

- ✅ **C-ABI FFI structs by value (flat scalar fields)** (2026-06-18, `auto-task/ffi-struct-by-value`)
  — an extern fn can take and/or return a C struct **by value** (not by pointer): name a Chezzi `struct`
  as a param/return type and its fields marshal in declaration order into a C-ABI struct layout. New
  `CType::Struct{name, field_names, fields}` in `src/native/cffi.rs` carries **only owned data** (no
  libffi `Type`, which is `!Send`/`!Sync`/`!Clone`) — the libffi structure type + per-field offsets are
  rebuilt per call via `ffi_get_struct_offsets` (platform ABI — small-struct-in-registers vs by-hidden-
  pointer — is libffi's, never hand-rolled), keeping `Cffi` `Send + Sync` for `--parallel`/M:N (made
  `CType` non-`Copy`; by-ref matching). A struct **param** writes its fields into a per-arg buffer at the
  libffi offsets (reusing the scalar `as`-casts incl. the fixed-width widths) via a new
  `Host::arg_struct_fields`; a struct **return** drops to the raw `ffi_call` with an own rvalue buffer
  sized `max(struct_size, sizeof(ffi_arg))` (the register-width floor from the narrow-int-return fix) and
  reads each field at its libffi offset into a `NativeRet::Struct` both engines already lower. `ctype_of`
  (compiler + interp, byte-identical) maps a struct `Named` to `CType::Struct` recursively with a shared
  visited-set (cyclic alias/struct ⇒ `None`, no overflow); interp pre-gathers a program-global
  `extern_struct_fields` like `extern_aliases`. **v1 = flat scalar fields only** — the checker rejects a
  struct with a `str`/nested-struct field (error naming the struct + field) and a generic struct; a
  `type P = Point` alias works like the bare struct. Golden `examples/ffi_struct.chz` binds
  `div_t div(int, int)` (pure libc; `{3, 2}`, byte-identical VM/`--interp`/`--parallel`); cffi round-trip
  unit tests (struct return + mixed long/double/long + fixed-width-field layout), checker + ctype_of
  parity tests. Docs: `syntax.md` §12b, `spec.md` §Level-3, `grammar.bnf`, `ffi-and-packaging.md`. Nested
  structs / `str` struct fields stay deferred.
- ✅ **C-ABI FFI width type names moved to `std.ffi` type imports** (2026-06-18,
  `auto-task/ffi-width-type-imports`) — the eight fixed-width integer TYPE names (`int8`..`uint64`) are
  **no longer global builtins**: they are now **imported per-name from `std.ffi`** (`import int32, uint32
  from std.ffi`) — **Chezzi's first type import**. `native::ffi::TYPE_NAMES` is the single declaring
  authority; `std.ffi`'s `ModuleSig.types` carries them, `bind_import` records each into a per-module
  `imported_ffi_types` set, and `resolve_type` maps a width name to `Ty::Int` **only** in a module that
  imported it (else *unknown type 'int32' (import it from std.ffi …)*). A bogus `import int99 from
  std.ffi` errors like any bad import. Both runtime engines' `from`-import binders **skip** the value-less
  width imports (parity by construction). Per-module: A's int32 struct field is usable from B with no B
  import; a width name written in B's own source needs B's import. **No runtime/marshalling change** —
  `cffi.rs` `CType` + both `ctype_of` untouched, the same C calls run, goldens byte-identical. FFI-special
  + minimal: NOT a general user type-export mechanism; `ptr`/`owned_str` stay bare builtins (NOTE:
  later superseded for `ptr` — see "task 2/5: FFI `ptr` gated behind `import std.ffi`" above; `ptr` now
  requires the import too, `owned_str` stays bare). Five new
  checker tests (no-import-rejected, import-then-extern+struct-ok, bogus-import, cross-module isolation
  ±), three existing FFI checker tests converted to `entry_ok` + import line, both goldens
  (`examples/ffi_int.chz` + `ffi_struct.chz`) gained the import line (`.expected` unchanged). 2202 tests
  green. Docs: `syntax.md` §FFI + §std.ffi, `spec.md` §Level-3, `PROGRESS.md`.
- ✅ **C-ABI FFI fixed-width integers — `int8`..`uint64`** (2026-06-18, `auto-task/ffi-fixed-width-ints`)
  — eight bidirectional integer marshalling type names (`int8`/`int16`/`int32`/`int64`/`uint8`/`uint16`/
  `uint32`/`uint64`) on the `extern "lib":` surface (later moved to per-name `std.ffi` type imports — see
  the entry above; **zero grammar/lexer/parser change**). Resolves the FFI-2 known
  limit (prior: *"scalars only — int ↔ long, no fixed-width int type"*). Each resolves to a plain `int`
  (`Ty::Int`) for the program; the width/signedness is a runtime-only marshalling distinction the backends
  recover via `ctype_of` (the platform-exact libffi `Type::i8()`/`u8()`/…/`i64()`/`u64()`; bare `int`
  keeps `c_long()` for back-compat). Unlike `owned_str` (return-only), these are **bidirectional**. C-cast
  boundary semantics, **no overflow trap**: a param **truncates** the Chezzi i64 to the C width (wrapping
  — `255` → `int8` is `-1`); a return **sign-extends** (signed) or **zero-extends** (unsigned) back to i64
  (`int32` `-1` → `-1`; `uint32` `0xFFFFFFFF` → `4294967295`). `uint64` above `i64::MAX` wraps negative
  (documented limit). Alias-safe: `type Len = int32` marshals as the int32 width (the alias resolves one
  hop into the leaf, placed before the alias fallthrough), and a cyclic alias still errors at the checker
  (no stack overflow). Eight flat `CType` variants + `ffi_type()`/param-cast/return-lower arms in the
  shared `Cffi::call()` (parity by construction); the two `ctype_of` sites (compiler + interp) mirror
  verbatim, guarded by twin tests. No C-spelling aliases (`c_int`) yet — width is platform-dependent,
  deferred. Five MockHost unit tests (round-trip, int8 truncation, sign-extend, unsigned zero-extend +
  high-bit), three checker tests (param+return for all 8, alias, cyclic-alias), twin `ctype_of` tests,
  golden `examples/ffi_int.chz` (atoi/htonl/abs) through both engines. ~2181 tests green.
- ✅ **C-ABI FFI `str`-return deepening — `owned_str` + `str?`** (2026-06-18, `auto-task/ffi-str-return`)
  — two paired, return-only opt-ins on the `extern "lib":` `char*` return path, implemented as **pure
  type-machinery (zero grammar/parser change)** — both ride a `Type` the backends' `ctype_of` recognizes,
  exactly like `ptr`. **(1) `owned_str`** (fixes the FFI-3 leak): a return-only marshalling type name
  (resolves to a plain `str` for the program) whose `char*` is copied into a Chezzi str **and then freed**
  with libc `free` (resolved once via `dlsym("free")` at `Cffi::new`, cached as a `usize`; best-effort —
  degrades to the old leak if unresolvable, never aborts). NULL still faults. **(2) `str?`** (`Option[str]`,
  already parses): a nullable `char*` — `NULL` → `None`, non-null → `Some(str)` — the opt-in escape from
  the non-null `str` faulting-on-NULL rule (kept byte-identical). Composes: `owned_str?` → nullable + owned.
  Three flat `CType` variants (`OwnedStr`/`OptStr`/`OptOwnedStr`), each `Type::pointer()` to libffi; both
  are **return-only** (a surface guard in the extern param loop + `assert_marshallable` reject them as
  params). Parity by construction (shared `Cffi`, `NativeRet::Some/None` already lower identically); the two
  `ctype_of` sites (compiler + interp) mirror verbatim. Golden `examples/ffi_str.chz` (strdup + getenv,
  byte-identical VM/`--interp`/`--parallel`); 4 cffi unit tests, 5 checker tests, 1 ctype_of test, 2 goldens.
  **Limits:** libc `free` only (a custom user-named deallocator stays deferred); `owned_str` is a user
  assertion the buffer is genuinely `malloc`'d (a static-string mis-declaration corrupts the heap). Docs:
  `syntax.md` §12b, `spec.md` §Level-3 (FFI-3 resolved), this file. `cargo test`/conformance green, clippy clean.
- ✅ **Comprehension nested clauses** (2026-06-17, `auto-task/comprehension-nested-clauses`) — a
  comprehension may now have 2+ `for` clauses (cartesian/nested iteration, first clause outermost,
  later clauses see earlier clauses' bindings), with one or more `if` guards allowed after ANY clause,
  across list/set/map forms (Python semantics). The `Comprehension` AST node now carries
  `clauses: Vec<CompClause>` (each `{ vars, iter, guards }`). VM folds the clauses right-to-left into
  nested `compile_for`s (reusing the for-loop lowering verbatim — no new bytecode); interp recurses
  left-to-right (`eval_comp_clauses`) for byte-identical iteration order + guard placement. Checker
  scopes progressively (per-clause `for_bindings`/`declare`, channel-drain rejection per clause).
  Grammar gains `<compClauses>`/`<compGuards>` (conformance green). `examples/comprehensions_nested.chz`
  + 5 cases asserted byte-identical on VM/`--serial`/`--interp`.
- ✅ **Comprehension stateful-iterator parity fix** (2026-06-17, same branch) — the interp now drives
  a comprehension's iterable LAZILY (`eval_comp_clauses` pulls one element, binds it, tests guards,
  then recurses/collects, then pulls the next), reusing the same per-element struct-`next()` loop as
  the `for` statement and the VM's `compile_for`. Previously it eagerly drained the iterator into a
  `Vec` first (via `collect_iter_rows`, now removed), so a comprehension whose element/guard read a
  stateful struct iterator's live field (`[x*100 + c.n for x in c]`) saw the fully-advanced state on
  the interp but the per-step state on the VM — a real two-engine divergence. This was **pre-existing
  for the single-clause form on `main`** (same eager `collect_iter_rows`); the nested form inherited
  it. List/map/set/str/range iterables are stateless, so their order/semantics are unchanged.
  `examples/comprehension_iter_state.chz` + interp/VM/golden parity tests.
- ✅ **`ref T` — transparent by-reference bindings** (2026-06-17) — a binding MODIFIER (locals + params
  only) that lowers to the existing `std.ref` `Ref[T]` box, **entirely in parser → checker → desugar**
  (no new runtime/VM op, so two-engine parity is by construction — all read/write/init lowering lives in
  `src/desugar/mod.rs`, run inside `resolver::build_graph`, which both engines + the checker consume).
  AUTO-DEREF (the user-approved design — no `^` operator, no call-site `ref` marker): a read `r` lowers
  to `r.get()`, `r = v` to `r.set(v)`, `r += 1` to `r.set(r.get()+1)`; init creates a fresh `Ref(v)` or
  ALIASES the same box when the RHS is already a `ref` binding. Coercion table enforced: `ref→ref` param
  aliases the box, `ref→T` param auto-derefs to a copy, a by-value local or a literal into a `ref` param
  is an error. `ref` is barred (parse error) from return types, generic args, collection elements, tuple
  elements, struct fields, and destructuring bindings; a `ref`-over-generic-param is a type error. Concurrency:
  a `ref T` is a `Ref[T]` → non-sendable, so crossing the airlock is rejected (matches `Ref[T]`; use
  `Shared[T]`). `ref` is now a keyword (corpus-safe; `import std.ref` paths still parse via a path-segment
  exception). Goldens `examples/ref_binding.chz` + `examples/ref_airlock.chz` (byte-identical on
  run/--serial/--interp); parser/desugar/checker unit tests + grammar.bnf REF terminal + corpus
  accept/reject fixtures. Docs: `docs/syntax.md` §3, `gaps.md` (RESOLVED), `docs/future.md` (item 12
  landed), `docs/concurrency.md`. `cargo test` green (2052+), `cargo test conformance` green, clippy clean.
- ✅ **`ref T` arg coercion is type-directed (indirect callees + closures + protocols)** (2026-06-17) —
  follow-up hardening the `ref` arg alias/deref/error decision so it follows the *resolved* callee, not a
  purely-syntactic name lookup. The decision still lives in `src/desugar/mod.rs` (it must — desugar runs
  inside `build_graph`, the one pass the checker and both engines share), but `callee_param_is_ref` now
  resolves indirect callees through local binding tracking: a LOCAL fn-value (`g := bump`/closure literal
  → `local_fn` flags) and a method call whose receiver's struct type is known locally (`x := S(...)` /
  `x: S = ...` → `local_struct`, looked up in a new `(struct, method)`-keyed spec map). Fixes (1) calling
  a `ref`-fn through a local fn-value (was a false `expected Ref[int], found int`), (2) a method name
  shared by structs that disagree on ref-ness (resolved by receiver type), (3) **closure `ref` params**
  (were silently inert) — now `bind_ref`'d in desugar and typed `Ref[T]` in `infer_closure`, so a `ref`
  arg aliases and a by-value arg is the same row-3 error as a named fn. (4) **Protocol `ref` params** are
  now honored (`Ref[T]`) in the protocol method sig so a conforming `ref` method matches. (5) Diagnostics
  for `ref` bindings render the `ref T` surface the user wrote (`ty::ref_display`), never leaking the
  lowered `Ref[T]`. Golden `examples/ref_indirect.chz` (byte-identical run/--serial/--interp); 13 new
  parser/desugar/checker tests. Known boundary: a method whose receiver's struct type is NOT statically
  known locally (e.g. `foo().apply(r)`) still resolves only when all same-named methods agree on ref-ness
  — otherwise it falls back to deref (the checker then gives a transparent `ref T` error). Docs:
  `docs/syntax.md` §3. `cargo test` green (2068), conformance green, clippy clean.
- ✅ **C-ABI opaque `ptr` handle for `extern "lib":`** (2026-06-18) — the first half of the FFI
  handle-unlock: a C library built around a `void*` handle (`FILE*`/`sqlite3*`/`create→use→destroy`)
  can now be driven over a dlopen'd `.so` with **no chezzi recompile**. New builtin opaque type `ptr`
  (↔ C `void*`), threaded through the whole pipeline: `CType::Ptr` marshalling in `src/native/cffi.rs`
  (arg + return; NULL return ⇒ `Ptr(0)`, **not** a fault, unlike `str`), `NativeRet::Ptr` +
  `Host::arg_ptr` in the seam, `Obj::Ptr(usize)`/`Value::Ptr(usize)` on both engines (GC leaf, no
  Drop, value-compared by address, `<ptr null>`/`<ptr>` stringify — **never** the raw address, which is
  non-deterministic across engines), sendable by value (`WireValue::Ptr`, fast-path snapshot),
  `Ty::Ptr` in the checker (marshallable + sendable; `ptr==ptr` only, no methods/fields/arithmetic).
  New **`std.ffi`** native module (`null() -> ptr`, `is_null(p) -> bool`) — the C value vocab lives in
  the library, not the language (no new keyword/literal). **Decisions:** untyped handles (one `ptr` for
  all — ctypes-level, C-UB on mismatch) + **manual free** (no auto-Drop → parity-clean; leaks if you
  forget, like FFI-3) + allow-NULL. Golden `examples/ffi_ptr.chz` (byte-identical VM/`--interp`, uses
  `/dev/null` + a bad path so it needs no writable fs); cffi unit tests (tmpfile/fclose round-trip,
  NULL-non-fault), checker tests, `std.ffi` unit tests. Docs: `syntax.md` §12b + stdlib, `spec.md`
  §Level-3, `ffi-and-packaging.md` (C half shipped; Rust `Arc<dyn Any>` userdata still forward-design).
  The Rust compiled-in handle (Burn) + registry stay deferred. `cargo test`/conformance/clippy green.
- ✅ **Checker control-flow boundary for `spawn:`/`defer:` blocks** (2026-06-16) — fixes a three-way
  divergence where `break`/`continue` lexically nested in an enclosing loop but placed inside a `spawn:`
  or `defer:` block passed `check`, raised `break outside loop` at runtime on the VM, and was silently
  treated as a block exit by the interp. Both block arms now save-zero-restore `loop_depth` around the
  body check (mirroring `check_fn_body`/`infer_closure`), so the existing `loop_depth == 0` guard rejects
  at check time with the uniform diagnostic; a legitimate loop INSIDE the block stays legal. Checker-only
  (no VM/interp/compiler edits); two-engine parity restored (runtime paths now unreachable from checked
  source). 4 rejection + 3 positive-guard tests in `src/checker/tests.rs`.
- ✅ **Adversarial-review remediation — `wait`/timer + C-ABI FFI** (2026-06-13, merges `b697ce0` (wait) +
  `e9dc3c1` (ffi)) — fixes the 8 findings from an adversarial review of the freshly-merged `wait`/`select`
  and FFI features, run as two file-disjoint auto-task worktrees (post-merge-gated, both `ship`; 1801 tests).
  **WAIT (vm only):** the `--parallel` `wait` lost-wakeup — a live `timer(N)` arm + live channel arm with
  nothing ready inline-`thread::sleep`d the worker and unconditionally took the timer, stranding a sibling
  `send` that landed mid-window (HIGH) and pinning the OS worker (MEDIUM). Fix = **full timed-park**: arm one
  background `timer::submit_at(deadline, send_wake(true))` on the soonest timer arm's own channel and fall
  through to the existing snapshot-park, so the `WaitPark` claimed-CAS sweep picks exactly one of {a sibling
  send/close, the timer's deadline send}; demote path (`native_reentry>0`) threads the deadline into the
  bounded poll. An **arm-once `ChannelCore.timer_armed` CAS latch** stops a re-park (woken by a `close` with
  no value) re-arming a redundant job (adversarial low finding). Cooperative VM + interp inline-sleep
  unchanged (parity oracle, `--parallel`-only + licensed-nondeterministic; 5 new VM tests, 600-race stress).
  **FFI (checker/parser/native/docs):** reject an `extern fn` colliding with a builtin/`print`/constructor
  or a struct/variant name (was silently shadowed → dead extern + startup `dlsym` abort) — order-independent,
  and corrected to NOT reject enum *type* names (not callable, so reachable; adversarial fix); reject
  non-top-level `extern` at the parser + grammar (was skipping marshallability validation); gate `cffi`
  `#[cfg(unix)]` (LLP64 `c_long` truncation now unreachable; project is unix-only); documented v1 limits
  (int↔C `long` width, malloc'd `char*` leak, non-reentrant C under `--parallel`).
- ✅ **Level-3 dynamic C-ABI FFI (v1)** (2026-06-13, `feat/c-abi-ffi`) — reverses the documented
  non-goal. New `extern "lib":` indentation block of statically-typed C signatures (`Token::Extern` →
  `StmtKind::Extern{lib, fns}` → `parse_extern` mirroring `parse_protocol`; grammar `<externDecl>` +
  conformance corpus). New `src/native/cffi.rs` holds `Cffi` (`dlopen`'d `Library` + symbol as `usize`
  + per-call `Cif`) whose `call(&mut dyn Host)` reuses the **same** `Host`/`NativeRet` seam as the std
  modules, so VM + interp + `--parallel` emit identical output (structural parity). `extern` fns are
  module globals (`vm::Obj::Cffi(Arc<Cffi>)` via `Op::MakeCffi`/`CffiDef`; `interp::Value::Cffi`), so
  the normal call-dispatch + `infer_named_call` type-check paths work with zero call-site special-casing.
  Checker enforces C-marshallability (int/float/bool/str + void) on the **resolved** type (aliases OK).
  `Cffi` is `Send+Sync` (symbol as `usize`, `Cif` rebuilt per call — both libloading `Symbol`/libffi
  `Cif` are `!Send`); the M:N snapshot path shares the `Arc<Cffi>` (same address space, no re-dlopen).
  v1 = scalars only (structs/callbacks/varargs/userdata/`char*`-ownership deferred); extern stays OUT
  of `is_blocking` (a slow C call runs inline). Golden `examples/ffi.chz` (cos/sqrt/strlen) two-engine
  parity-tested + `cargo test cffi/conformance/golden_ffi` green; +`libffi`/`libloading` deps.
  **Post-review blocker fixes** (merge `0a5938d`, after adversarial reject): (1) `nil` is now a
  return-only type — rejected as a param (the backend's `ctype_of` has no nil case, so accepting it
  panicked every engine on a *checked* program); (2) compiler + interp now resolve type aliases
  **program-globally** (matching the checker), so a cross-module alias used bare in an `extern` sig no
  longer panics / silently-voids the return — backends use `and_then` (None ⇒ void) not `.expect`;
  (3) a `str`-declared return that comes back `NULL` now **faults** instead of silently yielding `nil`
  (was a static non-null-`str` soundness hole). +5 regression tests (checker nil-param, vm+interp
  cross-module-alias + explicit-`-> nil`-return, cffi NULL-str-fault). Merged over `wait_select`
  (2 union conflicts: `<compoundStmt>` grammar + compiler imports); re-verified on merged HEAD —
  **1790 pass, conformance 7, clippy clean**; post-merge-gate verdict **ship**.
- ✅ **Match or-patterns + nested nullary variants** (2026-06-13) — one new AST `Pattern::Or(Vec<Pattern>)`,
  no new opcodes. `p1 | p2 | ...` at the top of an arm AND in sub-positions (`(1|2, x)`, `Some(a|b)`);
  every alternative must bind the same variables (checker-enforced, clear error otherwise); a full enum
  or-pattern is exhaustive without `_`, but the open int/str/bool domains (incl. `true | false`) still
  need a `_` (one rule preserved). Nested nullary variants (`Some(None)`, `Ok(Err(e))`) are now refutable
  variant matches — checker promotes a bare nested capitalized ident via the variant registry; compiler +
  interp route by the same registry so all three engines agree (golden `examples/match_or.chz` byte-
  identical on VM / `--interp` / `--parallel`). Grammar `<pattern> ::= <patternPrimary> ("|" ...)*`;
  `cargo test conformance` green.
- ✅ **D6c — per-socket read/accept/write timeout** (`--parallel`) — `read(n, timeout_ms)` /
  `write(s, timeout_ms)` / `accept(timeout_ms)` → `Err("timeout")`; reuses the deadline-bounded poll, no
  new thread/heap/job. In-callback (Path-C) timeout out of scope v1.
- ✅ **D6a/D6b — netpoller + non-blocking `std.net`** — epoll/kqueue poll thread (`src/vm/poller.rs`)
  turns a would-block socket op into a fiber-park; `Obj::Socket`/`Obj::Listener` over `Arc` cores; true
  non-blocking `connect` (`socket2`); drain-on-fault re-injects socket-parked fibers; timer folded into
  the poll thread. Echo server services 100 conns ≫ workers in one `parallel:`.
- ✅ **D5 — dirty/blocking pool** (+ owes #1–#3) — a blocking off-heap-safe native suspends the fiber and
  hands the call to a growable pool instead of pinning a core worker; process-wide timer thread for
  `sleep_ms`; `request`/`process` classified blocking; `iter.*` HOFs (chezzi source) let a `recv` in a
  callback park; **Path C** demotes the worker (one raw replacement thread) for a `recv`/`sleep`/socket op
  reached inside a native callback. Residual #2 (executor-spanning demote) WON'T FIX by design.
- ✅ **D4 (a–e) — Go-style work-stealing** — per-worker local run queues (`LocalQ`) + shared global
  overflow + random-victim steal-half + periodic global check; runnable-gated park wake (the mutex *is*
  the StoreLoad barrier — no Go fence). The conditioned single-wake (`notify_one`) is a deferred
  throughput-only refinement.
- ✅ **D3 — reduction-counting preemption** (BEAM-style) — a fiber's `reds` budget yields at exhaustion to
  the run-queue tail, so a CPU-bound fiber can't starve siblings; the yield unwinds every nested
  `run_until` level via a `paused()` helper.
- ✅ **D2a/D2b — M:N scheduler** — lightweight share-nothing fibers (own heap in a swappable `FiberCtx`)
  multiplexed over the bounded pool, **parking on `recv` instead of pinning OS threads**; exact
  single-coordinator deadlock predicate; the inline join shell alone guarantees completion (decision B).
- ✅ **D1 — lazy module snapshot** — a shared read-only `Arc<ModuleSnapshot>` faulted into each worker
  heap on first access, killing the per-task module-graph rebuild.
- ✅ **D0 — O(N²)→O(N·logN) cooperative ready-queue** — per-nursery `ready` set + parked-index buckets,
  keyed by `ChannelCore` pointer; 50k fibers: seconds → tens of ms.
- ✅ **Per-connection `spawn`** — eager injectable nursery so a nested `parallel:` `spawn` runs
  concurrently with the rest of the body (the canonical accept-loop server shape). v1: ≥2 cores, bounded
  accept loops.
- ✅ **`Channel.close()` + `try_send` + `for v in ch:`** — clean producer→consumer termination, closed-
  channel fault semantics, channel-iteration (both engines); comprehension-over-channel checker-rejected.
- ✅ **Pending-`spawn`-drop on early `parallel:` escape** — unstarted tasks cancel-and-report on
  `?`/`return`/`break`/`continue` before the join (both engines, parity-restored).
- ✅ **B3.6 — `Executor` on the pool + A3b `submit`-capture gate** — submitted closure crosses by value
  on BOTH engines (`WireValue::Closure` via `wire_callable`; the cooperative by-handle path was retired
  once the `interp` oracle was removed — see "Executor.submit coop==M:N by value").
- ✅ **B3.4/B3.5 — cancellation + cross-thread `os.exit` + thread deadlock detection** — per-nursery
  `cancel` flag (first fault/exit trips it; `os.exit` wins; cancel bypasses `recover:` but runs `defer`s).
  Single-level cancel only (nested propagation deferred).
- ✅ **B3.3 (a–d) — `str`-by-value + G1 module-globals checker gate + worker module-graph reconstruction +
  real OS threads behind `--parallel`** — mutating a `spawn`-reachable module global is a checker error
  ("use Shared[T]"); bounded pool, parent participates inline.
- ✅ **B3.0–B3.2 — `WireValue` airlock + cores into `Arc<…Core>` + `Arc<Program>` + isolated worker VMs**
  — `deep_clone` → wire round-trip; `Channel`/`Shared`/`Executor` cores out of the heap; cross-heap safety
  enforced (`ensure_crossable`). All single-thread, byte-identical. See `docs/concurrency-b3.md`.
- ✅ **Concurrency A1 — `Channel.try_recv() -> T?`** — non-blocking poll (both engines), un-deferred once
  B1/B2 landed.
- ✅ **Concurrency C5 / Group B — B1 + B2 cooperative fibers + blocking `recv`** (VM) — suspendable
  execution: a `recv` on an empty channel parks the fiber and the nursery-local scheduler runs a sibling.
- ✅ **Concurrency C5 — `Executor` escape hatch** + **A2 program-exit auto-drain** + **A3a** (pinned) — the
  sequential-subset `Executor()` / `submit` / `shutdown[_now]`, drained at clean exit (both engines).
- ✅ **Concurrency C4 — VM parity for `spawn`/`parallel:`/`Channel`/`Shared`** — ported C1–C3 onto the
  default bytecode engine (heap objs, ops, VM `deep_clone`, sequential nursery executor).
- ✅ **Concurrency C3 — `Shared[T]`** (interp) — cross-task mutable box (`get`/`set`/`update`); handle
  sendable, `Ref[T]` forced non-sendable.
- ✅ **Concurrency C2 — `Channel[T]` + sendability** (interp) — buffered FIFO mailbox; a `sendable(Ty)`
  predicate gates element types, `spawn` args, and capture reassignment.
- ✅ **Concurrency C1 — `spawn` / `parallel:` nursery** (interp, sequential executor) — structured
  concurrency; `spawn f(x)` and `spawn:` block run to completion FIFO at the dedent.
- ✅ **Integer overflow policy** — every `i64` overflow is a recoverable fault (never wrap/crash).
- ✅ **Gaps pass II** — `Ref[T]` mutable box (`std/ref.chz`); `sort_by_key`; call fn-typed field
  `self.f(x)`; relaxed non-const defaults; runtime stack traces (both engines).
- ✅ **String format specifiers** (6th/last of the f-string ergonomics batch) — Python-style
  `{expr:[[fill]align][sign][0][width][.precision][type]}` after a `:` in interpolation. Type chars
  `d f x X b o e %`; string `.N` truncates. **Width/precision capped at 4096 at parse time** (fixes a
  prior OOM from unbounded `repeat`). Spec parse+format is a single shared module `src/fmtspec.rs`
  (`split_spec`/`parse`/`apply` + neutral `FmtArg`) routed through BOTH engines (`Op::ToStrFmt` in the
  VM, `interp::interpolate`) → byte-identical output. `:`-split is bracket/quote-aware (`{m["a:b"]}`,
  slices). Unknown type char = compile error; type/value mismatch = runtime error (same message both
  engines). Golden `examples/format_specs.chz` parity-checked VM/interp/--parallel.
- ✅ **Scripting-ergonomics gap pass** — hex/bin/oct literals; list `.concat`/`.extend` + map
  `.merge`/`.update`; tuple-destructuring `for` + `enumerate`/`zip`; `?.` + `??`; tuple destructuring +
  match-on-tuple + guards.
- ✅ **Fix — loop variable is immutable** — checker rejects assigning a `for`-loop var (was a VM/interp
  divergence); inner `:=` shadow stays mutable.
- ✅ **M18 — `defer` → block/lexical scope** — runs when its enclosing block exits on every path, LIFO,
  inner-block-first. Supersedes M17.
- ✅ **M17 — `defer` (Go-style, frame-scoped)** — runs at frame exit, LIFO; receiver+args evaluated at the
  `defer` statement.
- ✅ **M16 — comprehensions + `std.os.exit(code)`** — `[e for x in it if g]` (+ set/map forms),
  first-class AST node; hard uncatchable cooperative exit.
- ✅ **M15 — slicing + `Index`/`IndexSet`/`Slice` protocols** — **Python-style** `xs[a:b:c]` (open bounds,
  step, reverse `[::-1]`, bounds-clamped) + **negative indexing** `xs[-1]` (plain index faults out of range,
  slice bounds clamp — Python's asymmetry); the `..` operator stays the for-loop/match range. list/map/str
  intrinsic, user structs structural via `slice(self, start: int?=None, end: int?=None, step: int?=None)`.
  (Originally shipped as Rust-range `xs[a..b]`; migrated to colon syntax — see "Slice syntax → Python colon"
  below.)
- ✅ **M14 — method-level type params** · user-defined parameterized protocols · default + named args on
  methods (desugar-pass).
- ✅ **Default + named arguments** — free fns + struct ctors; scope-aware desugar pass, both engines
  consume a normalized AST.
- ✅ **Tech-debt sweep** — reject dup generic param `[T, T]`; nested `set` equality parity; explicit
  call-site type args `name[T,…](…)`.
- ✅ **M11 — panic recovery + Go-style errors** — 2-param `Result[T, E]` (`T!`/`T!E`), `Error` protocol,
  `recover:` boundary catching any transitive runtime fault.
- ✅ **M10 — type-system depth** — `Stringable`/`Hashable`, per-operator `Add`/`Sub`/`Mul` protocols,
  multi-bound `T: A + B`, transparent aliases, generic enums; `map`/`set` reworked into insertion-ordered
  hash tables.
- ✅ **M9 — Tier-2 stdlib** — `std.regex` (`regex` crate) + `std.request` (`ureq`+rustls, blocking).
- ✅ **M8 — Tier-1 stdlib** — iterable strings + `chars()`; `std.json` (pure-Chezzi + `decode[T]`); native
  `std.process`/`std.fs`/`std.time`; `set` type.
- ✅ **M7 — generics + structural protocols** — type-erased generic fns/structs, Go-style `protocol`s,
  `Comparable`; `std.cmp`; `list.sort()` widened.
- ✅ **Round 2 gaps #10–#15** — `sort_by`, `ord`/`chr`, int+float math, map `for`, nested/tuple match,
  bitwise ops; iterator protocol (`next()`), `Iterator[T]` bound + lazy adapters, match guards +
  half-open range patterns.
- ✅ **Tuples + multiple return + destructuring (gap #8)** — `(e1, e2, …)`, tuple types, `a, b := f()`,
  `.0`/`.1`; immutable, fixed-arity, GC-traced.
- ✅ **M6a/b/c** — core-type str/list methods; pipe `|>` (parse-time desugar); stdlib via the Level-2
  native FFI seam (`std.math`/`std.io`/`std.os` native, `std.str` pure Chezzi).
- ✅ **`Map[K, V]` dictionary (gap #5)** — literals, keyed read/insert/update, six methods, GC-traced.
- ✅ **Index & field assignment** — `xs[i] = v`, `p.x = v`, `+=`/`-=` in place (both engines).
- ✅ **M5a/b/c** — bytecode compiler + stack VM; hand-built mark-sweep GC; cross-engine parity + perf;
  CLI default flip to the VM (`--interp` for the tree-walker). `read_file` capped at 64 MiB.
- ✅ **M4.5 — modules / imports + resolver** — multi-file, `chezzi.toml` root, run-once dep order,
  cross-module home-globals, cycle detection; program-global type names.
- ✅ **M4 — type checker (local inference)** — bidirectional, no unification; return-type inference,
  `T?`/`T!` sugar, expression-valued `match`/`if`, Go-style error accumulation.
- ✅ **M3 — tree-walk interpreter** — full expr/stmt set, `?` operator, interpolation, 256 MB-stack thread
  + `MAX_CALL_DEPTH` guard.
- ✅ **M2.5 — canonical grammar + conformance** — `docs/grammar.bnf` executed via the `bnf` crate,
  differential-tested vs the parser. `cargo test conformance`.
- ✅ **M2 — parser → AST** — recursive descent + Pratt; spans; depth-capped.
- ✅ **M1 — lexer** — full `examples/hello.chz` incl. Indent/Dedent; string escapes, numeric underscores.
  Shipped follow-ups: scientific-notation floats (`1e3`/`1.5e-9`/`6.022e23` — any exponent ⇒ float;
  bare `e` not half-consumed), single-quote strings (`'…'` ≡ `"…"`, same escapes & interpolation),
  unicode `\u{HEX}` escapes (1-6 hex digits, rejects surrogates/>10FFFF/malformed). Golden:
  `examples/literals.chz` (VM + interp + `.expected`).

---

## Stdlib additions (post-M18, 2026-06-13)

Additive-only, two-engine-parity-clean library surface landed alongside the M19 perf freeze (the freeze
is on *language semantics/syntax*; these add functions without changing any existing behavior). Built in
3 parallel `auto-task` worktrees, merged A→B→C with a `post-merge-gate` pass (verdict **ship**; one
cross-task semantic merge conflict — a test-mock `Host` impl missing the new trait method — caught at
compile and fixed). All TDD'd; suite at **1630 green**.

- **`std.math`** — trig/exp/log intrinsics: `sin cos tan asin acos atan atan2 exp ln log2 log10 log`
  (native, `src/native/math.rs`; plain `Float` pass-through — domain errors yield NaN, no `Result`
  wrapping, matching the minimal additive design). Golden: `examples/math_more.chz`.
- **`std.str`** (pure-Chezzi, `std/str.chz`) — `ends_with index_of count replace strip_prefix
  strip_suffix`, built only on existing native str methods. Golden: `examples/str_more.chz`.
- **`std.iter`** (pure-Chezzi, `std/iter.chz`) — `take drop any all find flatten`, in the existing
  fiber-park-safe generic style. Golden: `examples/iter_more.chz`.
- **`std.request`** — non-GET/POST verbs `put`/`patch`/`delete`/`head` + a general
  `request(method, url, body, headers: Map[str,str])` for custom headers (`src/native/request.rs`).
  Required a cross-engine `Host::arg_str_map` and a new **`NativeArg::Map`** variant so the
  headers-carrying form stays in `is_blocking()` and offloads to the `--parallel` dirty pool without
  pinning a core worker. Two-engine parity locked by `request_verbs_and_headers_parity_against_local_server`.
- **Considered, not built:** `json.decode[T]` — already shipped (`src/json_decode.rs` + parser/compiler/
  checker); first-class compiled `Regex` — deferred, blocked on Level-3 Userdata (see `docs/spec.md`).

## Syntax ergonomics (post-M18, 2026-06-13)

Token/parser-level only — two-engine parity is by construction (both engines call `lexer::tokenize`
then `parser::parse`; interp untouched). TDD'd, conformance + clippy clean; suite at **1642 green**.

- **Multi-line collection literals** — the lexer gained a `bracket_depth` counter; while `>0` it
  suppresses layout (Indent/Dedent/Newline) so `[]`/`{}`/`()` literals, call args, and param lists
  can span lines (`src/lexer/mod.rs`). Stray closer clamps via `saturating_sub`; the suppressed-
  newline path always `advance()`s past `\n` and `continue 'scan`s (never recurses) so an unclosed
  bracket terminates at `Eof` — guarded by the `unclosed_bracket_terminates_at_eof` tripwire (a prior
  attempt OOM-killed the box by spinning the tokenize loop on malformed input; this is the invariant).
- **Optional trailing comma** — one trailing `,` before the closer on list/map/set/tuple literals +
  call arguments + fn/closure params (`[1,2,]` ≡ `[1,2]`; lone `[,]`/`(,)`/`f(,)` still error).
- **One-element tuples** — `(x,)` is now a 1-tuple (was rejected); `(x)` stays grouping. Flipped the
  `reject/one_element_tuple` corpus → `accept/`, added `accept/trailing_comma.chz`, and relaxed the
  `<primary>`/`<params>`/`<argList>` productions in `docs/grammar.bnf` (conformance green). Golden:
  `examples/multiline_literals.chz` (VM == interp == `--parallel`).

## QoL syntax batch (post-M18, 2026-06-14)

Four ergonomics features, each a vertical TDD slice through lexer→parser→checker→compiler/vm + interp,
VM == interp == `--parallel` on every registered example. Conformance + clippy clean; suite at **1902 green**.

- **`in` membership operator** — `x in xs` → `bool`: list/set element, map **KEY** (Python-style),
  str substring. `BinaryOp::In` at comparison precedence (level 7 == `==`); `for x in xs:` is
  unaffected (the parser consumes `in` explicitly there). New `Op::Contains` + `op_contains` helper
  (reuses `values_equal`/`hash_key_rooted`/`candidates` — the same machinery as `.has`/`.contains`);
  interp `eval_binary` scans linearly with `values_equal_guarded`. No user `Contains` overload.
  Example: `examples/membership.chz`.
- **Compound assignment** — `*= /= %= &= |= ^= <<= >>=` (joining `+= -=`), all desugaring to the
  existing binary ops via `AssignOp::to_binop()` (shared by compiler + interp). Arithmetic forms
  numeric (no int-slot widening — `int /= float` rejected); bitwise forms int-only. Works on var /
  index / field / map-value targets. (`//=`/`**=` excluded — no `//`/`**` base op yet.) Example:
  `examples/compound_assign.chz`.
- **Triple-quoted strings** — `"""…"""` / `'''…'''`, lexer-only. Same escapes + interpolation as a
  regular string; the only added power is unescaped quotes inside. Produces a normal `Token::Str`, so
  everything downstream is unchanged (parity by construction). Example: `examples/multiline_str.chz`.
- **Multi-target / tuple-swap assignment** — `a, b = b, a` (also `data[0], data[1] = …`, struct
  fields, and `a, b = f()` for a tuple-returning `f`). Parser collects a comma lvalue list before
  `=` (op `=` only — compound with multiple targets is a clean parse error); the full RHS is
  evaluated into a hidden temp FIRST (Python semantics — correct even when an index appears on both
  sides), mirroring the destructuring-binding lowering. Example: `examples/tuple_swap.chz`.

> One sharp edge found + fixed: adding the `Op::Contains` arm to the VM's `step` grew its frame just
> enough to trip `self_referential_stringable_hits_depth_limit` (infinite `str(self)` recursion must
> hit the 10_000 call-depth limit before exhausting the host stack). Dispatching with `return
> self.op_contains(span)` instead of `… ?` keeps `step`'s frame from materializing the extra
> `RuntimeError` temporary. Grammar (`<eqExpr>` + IN; `<assignStmt>` + 8 compound ops + tuple alt) and
> conformance corpus updated; `cargo test conformance` green.

## Roadmap (later)

- VM/GC optimizations beyond M19 — NaN-boxing (own milestone), register VM, generational/incremental GC,
  Cranelift AOT/JIT. Written up in [`docs/future.md`](docs/future.md).
- **Bug-discovery track (pre-JIT)** — automated bug finding. ✅ **CPython output-differential built**
  (`src/difftest/`, see Current focus). Remaining: cargo-fuzz parser (lever #1), Miri/sanitizers,
  proptest, metamorphic. Ranked plan + rationale in [`docs/bug-discovery.md`](docs/bug-discovery.md).
  Recommended to stand up Tier 1 before the JIT, so the reference semantics are fuzzed + differentially validated first.
- ~~**M-C — implicit nurseries**~~ — **shipped 2026-06-12** (see Concurrency above).

### Ideas — record-only (not scheduled)

- **Native FFI / Rust-library bindings** — let Chezzi call into Rust libs; design sketch in `docs/spec.md`
  → *Standard library* → "Future idea — native FFI". **Dynamic C-ABI FFI v1 has since shipped** (`extern
  "lib":` scalar calls via dlopen+libffi — see "Done" below; **plus opaque `ptr` handles, `char*`
  ownership (`owned_str`/`str?`), flat-scalar structs by value, and sync scalar callbacks — all
  shipped**); remaining surface (nested structs-by-value, `str` struct fields, stored/cross-thread
  callbacks + pointer-deref builtins, varargs, the rich Rust `Box<dyn Any>` userdata handle) is still
  deferred.

---

## Known friction / open (document-only)

Surfaced by coverage passes; no `src/` changes pending, recorded for when they bite:

- **`match` limits** — no multiple `Some(...)` arms (one arm per outer variant; refine with `_`).
  Nested nullary-variant patterns (`Some(None)`, `Ok(Err(e))`) and **or-patterns** (`p1 | p2`) now
  work — see below.
- **Float arithmetic is total IEEE-754** (landed): float ops never fault — `1.0/0.0`→`inf`,
  `-1.0/0.0`→`-inf`, `0.0/0.0`/`5.0%0.0`→`NaN`, `math.sqrt(-1.0)`→`NaN`. `inf`/`NaN` are values;
  inspect with `math.is_nan`/`math.is_inf`/`math.is_finite`. **Integer** arithmetic still faults
  (overflow, `/0`, `%0`), and casting a non-finite float to `int` still faults. **Ordered
  comparisons involving `NaN` are total too** (landed): `< <= > >=` against a `NaN` always return
  `false` (never fault), matching IEEE-754 / Python / Rust; equality is unchanged (`nan == nan`→
  `false`, `nan != nan`→`true`). `sort()` and `sort_by_key` are **deterministic** with `NaN` keys —
  a total order (`f64::total_cmp`, `NaN` sorts to one end), never a fault.
- **`std.os.getcwd`** not yet injectable via `HostConfig` (parity holds); **`read_file`** capped at 64 MiB.

## Notes

- Recursive structs "just work" via the checker's two-pass name collection — trees and linked lists need
  only `Node?` child fields + a `match` per step, no special support.
