# Chezzi — gap ledger

Open gaps only. One row per open item, each **re-verified on the release binary** on the date in the
Verified column. Everything closed — 159 struck rows and all 30 bug-hunt session logs, W1 through
W14 — lives in **[`docs/gaps-archive.md`](gaps-archive.md)**, which is a byte-for-byte carry-over of
this file's previous content, so historical `docs/gaps.md:NNNN` citations in closed tickets and in
`PROGRESS.md` resolve there unchanged.

Rules that govern this file (unchanged):

- A row is closed by striking it in the archive, never by deleting it; a new finding gets a new row,
  not a resurrected old one.
- A fix is judged against the **owning ancestor** — Go for concurrency/interfaces, CPython for
  scripting feel, Rust for enums/errors — **run**, not recalled. There is no second engine to diff
  against.
- Read the wave's session log in the archive before working any row; several share one fix.
- Read a closed row's **prescription** before re-implementing it. Two of them (W8-2, W8-7) were
  measured wrong after filing.

**Where the waves are, in the archive:** wave 14 `:13307` · wave 13 `:12849` · wave 12 `:12616` ·
wave 11 `:12485` · wave 10 `:380` · wave 9 `:409` · dogfood wave 2 `:519` · dogfood wave 1 `:685` ·
serial-engine removal `:1068` · wave 7 `:1459` / `:5499` / `:5969` · wave 6 `:1503` ·
waves 2–5 `:3057`–`:3286` · the 2026-07-14 four-axis audit `:3568`.

---

## TABLE — **10 open rows** (all re-verified 2026-09-20; W11-15 closed 2026-09-21; W11-13 closed 2026-09-22; W15-1 closed 2026-09-22; W15-2 found 2026-09-22; W15-3 found 2026-09-23; W15-4, W15-5, W15-6, W15-7, W15-8 found 2026-09-23)

| row | P | domain | what | verified 2026-09-20 | archive |
|---|---|---|---|---|---|
| **W8-19** | P2 | affordances | Bundle. Remaining: multi-statement closures (ranked first, own milestone), `path.join` rejects `Path`+`str` / no `path.abs`/`path.rel`, statement-only `recover:`. Global helpers and the `Option` half are DECLINED, do not re-file (the `Result` half closed under TICKET-039). (The `Listener` item closed 2026-09-22, TICKET-166: `close()` cancels a parked `accept`, Go's shutdown idiom.) | **OPEN** | `:185` |
| ~~**W11-13**~~ | P3 | airlock | The isolation warning gated on the READ shape. **CLOSED 2026-09-22 (TICKET-165).** The taint now records each task write's constant field/key path, and a parent read along that path, a prefix of it or an extension of it warns; a mutator on an element or field (`xs[0].push(v)`) is a task write too. Pinned by `a_same_path_read_of_a_projected_task_write_warns`. | **CLOSED** | `:12510` |
| ~~**W11-14**~~ | P3 | cancel | `Vm::guarded_checkpoint` (`src/vm/exec.rs:385`) has the owner hole TICKET-096 fixed at the other two checkpoints. Condition to re-open: the checkpoint runs per ELEMENT and `MnSched::scope_fault` takes the sched lock, so a rung there needs its own `benches/run.chz` measurement. | **CLOSED 2026-09-21 (TICKET-155)** — the rung rides the 1-in-1024 `back_edge_tick` sample and `hof_nursery` measured level; see `docs/benchmarks.md` | `:12511` |
| ~~**W11-15**~~ | P3 | airlock | The three `RwShared` stores (`Op::NewRwShared`, `RwShared.set`, `RwShared.write`) split a DAG alias into two copies. **CLOSED 2026-09-21 (TICKET-154).** All three now serialize through `to_wire_crossable`, and the four looping read views share one rebuild map taken under one guard, so the cliff `rwshared_view_over_shared_bindings_is_not_quadratic` stays green. Pinned by `airlock_rwshared_store_dag_alias_is_one_object`. | **CLOSED** | `:12512` |
| **W12-5** | P1 | airlock | Five of six spawn-crossing shapes closed (TICKET-111). The sixth, a **sent closure** (G6), is NOT a residual: owner decision D2 (DEC-137, TICKET-137, 2026-09-19) makes a received closure read the running task's own module globals, so `[1, 2] [1]` is the rule's answer, and CPython/Go's `[1, 2] [1, 2]` is a difference BY DECISION. TICKET-154 built the fix and withdrew it for exactly that reason (2026-09-21). **SUPERSEDED BY D4 (owner, 2026-09-22, `docs/decision-d4-airlock.md`):** a task-side write to a copy now faults, so G6 closes as "rejected by D4 rule 3" under TICKET-169/170. Earlier note — D2 is consistent but not good UX, and it diverges from both ancestors, so it may be reversed toward Go/CPython. Do not re-file it as a bug; reopen it as a design change to D2, and start from TICKET-154's withdrawn fix. Pinned by `airlock_closure_over_a_captured_alias_pushed_by_the_receiver_is_a_known_residual`. | **OPEN** | `:12633` |
| ~~**W13-27**~~ | P2 | perf | Introduced by TICKET-131's W13-6 fix. A nursery inside a spawned task costs ~1.8x while its ENCLOSING nursery's body is still open. Identified recovery path, not implemented: farm pool helpers for an outer sched while its body is blocked, not only after `close_body`. **Do not** recover it by restoring the private nested sched at T>=2 — that is W13-6. | **CLOSED 2026-09-21 (TICKET-159)** — blocked-body helpers | `:12894`, repro `:13233` |
| ~~**W15-1**~~ | P1 | net | Closing a `Listener` from another task while one task is parked in `ln.accept()` never unblocked the accept: the program hung (5/5 runs, `timeout 5` rc=124, release, default workers) and 2/5 runs also panicked `netpoller add: Os { code: 9, ... "Bad file descriptor" }` at `src/vm/poller.rs:269`. Go 1.27: `Accept` returns `use of closed network connection` at once. **CLOSED 2026-09-22 (TICKET-166).** `close()` on a `Socket`/`Listener` now takes the handle out of its mutex, sets a `closed` flag, then deregisters, then drops the handle; `poller::register` refuses a park once `closed` is set (checked under the same registry lock as the scope cancel). A racing `accept`/`read`/`read_bytes`/`write`/`write_bytes` now returns `Err("<op> on a closed listener\|socket")` instead of hanging or panicking. Pinned by `tests/exit_status.rs::closing_a_listener_wakes_a_parked_accept` / `::closing_a_socket_wakes_a_parked_read` and `tests/chz/stdlib/net_close_test.chz`. | **CLOSED** | found 2026-09-22 |
| **W13-28** | P2 | perf/test | DEBUG binary only. A rendezvous ping-pong whose pair is spawned from INSIDE a spawned task runs bimodally at `CHEZZI_THREADS=8`. The broken pin it caused was already replaced 2026-09-18; what stays open is the bimodality itself. | **not re-verified** — needs a debug test binary and a 10-run sample | `:12895` |
| **W15-2** | P2 | sched | At `CHEZZI_THREADS=1` a top-level `parallel:` whose body and one spawned task both burn CPU runs at 195% CPU (`chezzi-base`, 3 runs, `wall=6.93s user=13.44s`), where Go 1.27 `GOMAXPROCS=1` runs the same shape at 100%; program: `fn burn(n: int) -> int:` / `    s := 0` / `    for _ in range(n / 5000000):` / `        for i in range(5000000):` / `            s = s + i % 7` / `    return s` / `fn main():` / `    ch := Channel[int](1)` / `    parallel:` / `        spawn:` / `            ch.send(burn(30000000))` / `        x := burn(30000000)` / `        print(x)` / `    print(ch.recv())` / `main()` (`/` separates lines). Cause: the body runs on the main thread while `activate_eager_nursery` (`src/vm/sched.rs:1057`) starts the `chezzi-eager` drainer on `wid=1`, and W8-8's fix (`eager_joiner_runs_fibers`) gated only the joiner, not this pair. Consequence: TICKET-167's seeded T=1 replay of a top-level `parallel:` holds only at a measured rate, not byte-for-byte. | **OPEN** | found 2026-09-22 |
| **W15-3** | P1 | net | Found by TICKET-167's seeded-scheduler oracle at `CHEZZI_THREADS=1`, seeds `1,2,3,4,5,7,8,10,12,13,14,15` of `1..17` (12/16), 0/16 at `T=2`: `closing_a_socket_wakes_a_parked_op_with_an_err` in `tests/chz/stdlib/net_close_test.chz:75` — a `write` parked on a `Socket` (`timeout_ms=0`) that another task then `close()`s returns `Ok` instead of the `"write on a closed socket"` error TICKET-166 (W15-1) gave the `Listener.accept()` case. Reproduces 10/10 replays at seed 1, `CHEZZI_THREADS=1` (`CHEZZI_SCHED_SEED=1 CHEZZI_THREADS=1 chezzi test tests/chz/stdlib/net_close_test.chz`), unseeded default (28 cores) T=1 also 0/10 — this needs the seed to land. TICKET-166's `close()` fix (`src/vm/{core,mod,netio,poller}.rs`) covers `accept`/`read`/`read_bytes`/`write`/`write_bytes`, so the socket write path either races a different lock than the listener accept path, or the closed check races the parked write's own send. Not investigated further — fix is a separate ticket. | **OPEN** | found 2026-09-23 |
| **W15-4** | P3 | test infra | NOT a scheduler bug — the documented streaming-CLI contract itself. `chezzi run`'s cross-task print order is nondeterministic BY CONTRACT (`docs/concurrency.md` "Output ordering: streaming CLI vs buffered sink"), so `try_recv.chz`, `parallel.chz`, `parallel_cross_nursery_ok.chz`, `channel.chz`, `channel_block.chz` and `executor_autodrain.chz` diverge from their own `.expected` at a rate too low for `schedfuzz`'s unseeded `BASELINE_REPS=5` sample to bound reliably (measured 2026-09-23: `channel.chz` 11/60, `parallel_cross_nursery_ok.chz` 0/40 unseeded `CHEZZI_THREADS=2` — a real, pre-existing, ALREADY-documented race no bounded rep count is guaranteed to catch). On the pre-fix binary, `parallel_cross_nursery_ok.chz` seed=7 threads=1 flagged an `output` finding in all 3 of 3 available corpus-sweep logs (`/tmp/step6-sweep.log`, `/tmp/t167-sweep-1.log`, `/tmp/t167-sweep-5.log`), against a 0/10 rate on the post-fix binary (`--program`, which bypasses `KNOWN_TARGETS`) — the fix disables `check_output` for this program's class, so the finding no longer surfaces at all, by design. When the sample happens to agree, `schedfuzz` scores a stable baseline and a later seeded divergence reads as an `output` finding; `KNOWN_TARGETS` (`src/schedfuzz/mod.rs`) skips these six by file name on a default (no `--program`) sweep so a routine clean-tree run does not keep re-reporting the contract as a regression. Tracking row only — no fix, because there is nothing to fix. | **OPEN** | found 2026-09-23 |
| **W15-5** | P3 | test infra | Two `tests/chz` files gate a fast path behind a wall-clock ratio (`if not optimized_build(): return` before `assert d < N`, an `optimized_build()`-style probe): `regex_test.chz`'s `find_all_text_extraction_over_a_million_tokens_is_not_too_slow` and `nested_nursery_open_outer_body_test.chz`'s `fan_open`/`fan_flat`. Under `schedfuzz`'s own concurrent job load (many `chezzi` child processes contending for CPU) the ratio flips and the assert genuinely fails UNSEEDED too — measured 2026-09-23: `regex_test.chz` 1/10 unseeded under sustained sibling load on an otherwise idle box (`uptime` load average ~5.9), 0/30 unseeded serially with no sibling load. This is a pre-existing load-sensitivity in the wall-clock gate itself, unrelated to scheduling; `KNOWN_TARGETS` skips both by file name on a default sweep. Fix (separate ticket, low priority): make `optimized_build()` re-check itself after the timed section, or widen the ratio's margin. | **OPEN** | found 2026-09-23 |
| **W15-6** | P2 | sched | Found by TICKET-167's seeded-scheduler oracle: `for_over_a_channel_in_a_generator_driven_from_a_task_gets_the_value` in `tests/chz/spec/generator_channel_test.chz` hangs at `CHEZZI_THREADS=0` (the default worker count, 28 cores this box). Measured 2026-09-23: seed=4 reproduces 3/10, seed=6 5/10 (`CHEZZI_SCHED_SEED=<seed> CHEZZI_THREADS=0 chezzi test tests/chz/spec/generator_channel_test.chz`); unseeded 0/10 with and without sibling CPU load. The file's own header comment says the test's CPU `burn()` only makes the intended interleaving LIKELY, not forced (TICKET-136/W14-16), so this is plausibly an untested interleaving of the generator-resume-inside-a-spawned-task path hitting the same hazard from the other side. Not investigated further — fix is a separate ticket. | **OPEN** | found 2026-09-23 |
| **W15-7** | P2 | sched/cancel | Found by TICKET-167's seeded-scheduler oracle: `tests/chz/stdlib/cancel_test.chz` hangs at `CHEZZI_THREADS=0`. Measured 2026-09-23: seed=1 did not reproduce in 10/10 direct replays, seed=4 reproduced 4/10 then 10/10 on a later sample (`CHEZZI_SCHED_SEED=4 CHEZZI_THREADS=0 chezzi test tests/chz/stdlib/cancel_test.chz`) — rate is sensitive to host load, consistent with `T=0`'s real-thread perturbation being genuinely non-replayable (by design, `docs/bug-discovery.md` "Seeded scheduler oracle"); unseeded 0/10 under sustained sibling load. The file exercises nested cancel-propagation (`spawn derive_burst`/`spawn cancel_root`, `spawn c5_waiter`/`spawn c5_canceller`, `spawn cancel_mid_teardown`); which of those five scopes hangs is not yet isolated. Not investigated further — fix is a separate ticket. | **OPEN** | found 2026-09-23 |
| **W15-8** | P3 | test infra | `vm::tests::an_eager_wait_block_is_woken_by_its_arm_not_by_the_poll_timeout` (`src/vm/tests.rs:16191`) failed twice under `cargo test --lib`, both at host `uptime` load ~34-35 on 28 cores (concurrent pipeline activity, unrelated to this ticket's diff): `300 gated handoffs must each block on arm 0's condvar; got 149` on 2026-09-23, and `got 178` on 2026-09-22. The SAME code gave `4875 passed; 0 failed` twice at lower load. `.project/run-test.sh` already documents this exact test flaking under CPU contention inside its capped scope (`RUST_TEST_THREADS=8` measured 45% failure there); this is the same class at `RUST_TEST_THREADS=4`, so contention alone, without raising the thread count, is now enough to flip it — TICKET-167's commits touch no `src/vm` code (last `src/vm` change `0e2baa13`, before both failures), so this is a pre-existing load-sensitivity in the test's condvar-vs-poll-timeout race, not a regression. Could not re-verify on an idle box: `uptime` load stayed 33.4-35.1 across a 5-minute poll. Fix (separate ticket, low priority): widen the test's margin or gate it on host load like W15-5. | **OPEN** | found 2026-09-23 |

### W8-17 — CLOSED (all five sub-items; the row is out of the table)

Every cosmetic this row listed is closed. (e) and (f) were fixed by later tickets that did not come
back and strike them here; (c), (d) and (g) closed under TICKET-158 on 2026-09-21. Full history:
`docs/gaps-archive.md:151`.

| sub-item | 2026-09-21 | |
|---|---|---|
| (c) one `match`-arm variant typo prints three errors at one position | **CLOSED** | TICKET-158: `E.Alpah` prints ONE error, `enum 'E' has no variant 'Alpah'` with `did you mean 'Alpha'?`, and no derived `non-exhaustive match`. A nested typo and the `Set`/`set` Hashable duplicate print once too; a non-exhaustive match whose ARM BODY or GUARD holds an unrelated error still reports both |
| (d) `unexpected an indented block in expression` | **CLOSED** | TICKET-158: reachable via a stray indent (`x := 1` then an indented `y := 2`); now `unexpected indented block in expression`, CPython's `unexpected indent` twin |
| (e) an unknown type renders as `?` | **CLOSED** | TICKET-145 rewrote the message: `'?' used in a function whose return type is not declared; declare it to return Result or Option (e.g. `-> int?`)` |
| (f) a `match`-pattern arm carets the scrutinee | **CLOSED** | TICKET-149 (W14-35b) gave arms their own span; the typo above now carets line 7, not the `match e:` line |
| (g) a literal's `end_col` is one char | **CLOSED** | TICKET-158: `x: int = "hello there"` → `"col":10,"end_col":23`; `word_end_col` now measures a string or number literal, so the LSP squiggle and the plain-text caret span it too |

### Repros, all run 2026-09-20 on `target/release/chezzi` at `50280d48`

    # W11-13 (CLOSED 2026-09-22, TICKET-165) — warns on `print("{s}")` and on `print("{s.v}")`
    struct S:
        v: int
    fn main():
        s := S(1)
        parallel:
            spawn: s.v = 2
        print("{s.v}")        # warns: 's' is read here as its pre-`spawn:` value
    main()

    # W11-14 (CLOSED 2026-09-21, TICKET-155 — the owner map is now cut short) — the child's fault never cuts the owner's straight-line callback short
    parallel:
        spawn: panic("child boom")
        ys := xs.map(fn(x) -> int: x * 2 + 1)   # 3 000 001 elements
        print("owner finished map after {…}")   # prints, 0.41 s, THEN the fault surfaces

    # W13-27 — fan_open vs fan_closed, release, 5 runs each (min/median, ms)
    T=8       fan_open  338/377    fan_closed  185/198
    default   fan_open  342/351    fan_closed  185/188

---

## Deferred tickets — 0 open, in `.project/tickets-deferred/`

Filed, triaged, parked. Closed 2026-09-21: **082** (tuple Map key, TICKET-161), **084** (TICKET-160 — the need is met by `m.copy()` and `Map(m.items())`; `Map(m)` itself stays a type error by decision, because a `Map` is `Iterable[K]` like CPython's dict and Go's one-variable `range`), and **087** (sub-second wall clock, TICKET-163 — `time.now_ms()`, `DateTime.milli`); **086** closed 2026-09-22 (TICKET-086 — `regex.find_all_text`); **083** closed 2026-09-22 — its nested-field half landed in TICKET-162 (`"{s:<{w}}"`, `"{x:.{p}f}"`) and `str.pad_right` / `string.pad_right` (CPython `str.ljust`) landed in place. The archive's decline of `pad_right` as "alias sugar" (`docs/gaps-archive.md:4122`) is superseded.

`.project/tickets-deferred/` also holds three `*.merged-into-*` stubs (078, 080, 088) — not work items.

---

## Pipeline

`.project/tickets/`: 156 done, 3 rejected (031, 127, 133), **0 in flight**. The wave-14 redesigns all
landed: **D1** a deadlock is fatal (TICKET-135), **D2** a received closure reads the running task's
module globals (TICKET-137), **D3** an int never widens into a float slot (TICKET-138).
