# Root causes — bug-hunt wave 21 (JIT sweep #6, 2026-10-06)

Sweep #6 ran six hunters on `main` at `d7519ef0`: airlock, cancel/exit/net, channels/scheduler/Executor,
checker/paths/generics, stdlib vs CPython, and FFI. Extra weight went on TICKET-208..218. The main loop
re-ran every finding below on the release binary. **Not clean: 2 P0 and 11 P1.** The JIT entry rule's
"two consecutive clean sweeps" count stays at zero.

The pattern this wave: the fixes of wave 20 each left a second copy beside their new single source.
- The Executor rebuild gave a job a sched slot, but a job's state still lives in three records (the sched,
  the Executor core, the Chezzi `Task` code).
- TICKET-210's one path resolver left the old call fallback writing the same node, and the one-argument
  bracket still reads its type through a second (expression) grammar.
- TICKET-215's one fn object per fn keyed the runtime, but the compiler's synthesized fns still have three
  ad-hoc memos, and decode has none.
- TICKET-213's CopyRead added a second marking walker.
- TICKET-218's range check was hand-placed beside about 15 of 56 assignability calls.

## Findings

| ID | Sev | Repro (short) | Chezzi | Ancestor |
|---|---|---|---|---|
| CK1 | P0 | `print((math.abs[int])(-6))`, `(lib.g[int])(6)` | check OK, run: `internal: two different name resolution decisions ... (W7-49)` | Rust `6` |
| CK3 | P0 | `json.decode[int] == json.decode[int]` | `false` (every other path value is `true`) | Rust `true`; syntax.md §Eq (TICKET-215) |
| C1 | P1 | `Executor(1)`; job 1 `os.exit(17)` while job 2 is held | job 2 starts; main prints `h2 Ok(2)` after the exit | CPython/Go: nothing after the exit |
| C1b | P1 | `Executor(1)`; job 1 panics (fire-and-forget) while jobs 2-3 are held | `job 2 start`, `job 3 start` after the run ended | the first fault ends the run |
| C2 | P1 | `Executor(1)`, held jobs, `shutdown_now()`, then `get()` | the held job never settles: `recv on an empty channel: deadlock` | CPython: held futures `CancelledError`; stdlib.md promises `Err` |
| CHAN3 | P1 | `t.done()` read while another task runs `t.get()` on a finished job | `done()` reads `false` after `true` (2095 times at T=2) | CPython 0 |
| CHAN4 | P1 | T=1: a job holds a `Shared` guard and waits on a channel; main takes the guard | 3/20 runs: the verdict faults the job, main runs past the lock | Go: always fatal at main's `Lock` |
| CHAN1 | P1 | two Executors, 4 CPU jobs each, `--threads=2` | 3.96 cores (pre-208: 2.0) | Go `GOMAXPROCS=2`: 2.0 |
| A3 | P1 | a generator writing its own frame-local list, read through an unobserved `Task.get()` in a copy | faults "this value is this task's copy" | CPython runs; spawn capture of the same generator runs |
| A4 | P1 | `g := math.sqrt; spawn g(9.0)` (also an extern fn value; also `ex.submit(time.now_ms)`) | check OK, run: `spawn: 'function' is not an isolable task` / `submit requires a function or closure` | Go `go g(9.0)` runs |
| S1 | P1 | `json.decode[Map[str, int]]`, `g[(int, str)]`, `Bx[Map[str,int]].make(..)`, `g[fn(int)->int]` | read as an index: `unknown name 'Map'` / parse error | Rust accepts |
| S2 | P1 | `Result[int, str].Ok(5)`, `Option[int].None`, `f := Some` | `unknown name 'Result'` / `'Option'` / `'Some'` | Rust accepts; spec.md:707 canonical form |
| CK5 | P2 | `(o ?? g)(5)`, `if c: inc else: g`, `[inc, g]` with a generic `g` | `'g' is generic and T is not determined` | Rust pins T from the sibling |
| FF1 | P2 | `m: Map[int8,str]; m[300] = "a"`; `b: Bx[int8] = Bx(300)`; `y: int8 = id(300)`; `z: int8 = 1 << 8` | check OK (runtime C boundary still faults) | Go/Rust: compile error `overflows` |
| C3 | P2 | `POST /r` → 303 → `GET /a`, the GET dropped on a reused connection | not retried: `io: Peer disconnected` | Go retries the GET hop: `Ok 200` |
| CK6 | P3 | `chezzi check bad.chz 2>&1 \| head -1` | rc=101 (Rust panic on a closed stderr) | `go vet` rc=1 |

Duplicates merged: CK2 = S1, CK4 = FF1, A1 = CK1, A2 = CK3, CHAN2 = C2. Repros: `~/.cache/hunt6/{air,cancel,chan,check,std,ffi}/`
(`check/m/C1.chz`, `check/p/D1.chz`, `check/p/n16.chz`, `check/p/W1.chz`, `cancel/p/x6.chz`, `j1.chz`, `s1.chz`,
`chan/p/td2.chz`, `G.chz`, `w8.chz`, `air/p/a3.chz`, `s2.chz`, `std/S1.chz`, `S2.chz`, `ffi/w/f1.chz`,
`cancel/http/c3.chz` + `srv.py` + `run.sh`).

Clean this wave:
- No stale spawn view after about 20 mutation doors between spawns.
- About 35 kinds of write to a task copy all fault; none is silently lost.
- `os.exit` runs no defer and flushes stdout in every position tried.
- The request retry grid matches Go, except C3.
- The runner cache leaks no threads; the six seeded FIXTURES replay byte for byte.
- FFI nested structs and varargs: 130 randomized programs matched C; `float32`, `cast_*`, `load`/`store`.
- The path grid of TICKET-210 matches Rust, except the cells above. `Num`, the newtype removal, and
  width types in ordinary code are clean.
- The stdlib matches CPython across about 25 modules.

## Family E2 — a job's life has one owner (C1, C1b, C2, CHAN3, CHAN4, CHAN1)

**Status (2026-10-07):** C1, C1b, C2 and CHAN3 fixed by TICKET-219 (facts 1 and 2: one transition,
`SchedCore::job_event`, reads the run halt before it releases a held job; the handle's sealed
channel is the one outcome record). CHAN4 (fact 3, the verdict is a run halt) fixed by TICKET-223
(one latch, `QuiesceState::decide`, delivered through `run_exit_err`). CHAN1 (fact 4) is a separate
ticket.
The recursive-main finding (a loop-free recursion has no cut point) is fixed by TICKET-224: a run-wide
halt lands at every function entry. CHAN1 is TICKET-230.

**Facts:**
1. what state a job is in, and what its outcome is;
2. whether the run is still alive when a held job is released;
3. who the deadlock verdict ends;
4. how many runners run Chezzi code at once, process-wide.

**Deciders today:**
- **Job state and outcome: three records.**
  - Sched: `exec_held` / `admit_or_hold` (`src/vm/mod.rs:3352`); the release loop in `finish` (`mod.rs:4970`);
    `drop_held_jobs` (`mod.rs:5836`), which writes `TaskOutcome::Cancelled` into the slot.
  - Executor core: `shut` / `cancel` (`src/vm/netio.rs:4103-4214`).
  - Handle, in Chezzi: `run_outcome` writes the `out` channel only from the job body (`std/concurrency.chz:101-124`).
    `Task.core` is written by readers (`std/concurrency/task.chz:42-80`), and `_outcome` does `recv`, then
    `try_send` the value back, then sets `Done`.
  - So a held job dropped by `shutdown_now` is `Cancelled` in the sched but never settles its handle (C2,
    a regression of TICKET-148 / W14-13). `done()` reads `Pending` with an empty channel inside
    `_outcome`'s window (CHAN3).
- **Run halt.** The release loop reads only `exec_active < exec_limit`. It never reads the exit/fault cell
  (`quiesce`, `run_exit_err` `netio.rs:2365`). `record_deadlocked` drops held jobs, but exit and fault
  release them: two policies at the two slot-fill sites (C1, C1b).
- **Verdict victim: two judges.**
  - The party judge (`block_halt_check`, `netio.rs:2450`) faults the judging party.
  - The sched idle judge (`mod.rs:4001` → `flag_deadlock_leaves` `mod.rs:5915`) faults its own parked
    fiber, here the job.
  - A job's `Deadlocked` is not a run halt (`sched.rs:2561-2565`), so main runs on (CHAN4).
- **Width.**
  - The per-sched claim `runner_wids` / `claim_runners` (`mod.rs:2679`, `:3442`); DEC-211 calls it "the
    one owner", but it is per sched.
  - Executor runners skip the process budget `NestedDrainerSlot` (`sched.rs:1444-1451`, `:6495`).
  - The `RUNNERS` gate is active only at T=1 (`sched.rs:331`, `:1248`, `:1441`); main is never counted at
    T>1. Before TICKET-208, jobs ran on the process pool (`pool.rs:63`), which bounded the process at N (CHAN1).

**History:** about 25 patches, one cell at a time: TICKET-045, 118, 125, 128, 147, 148, 191, 194, 208
(`bbbafad1` remap), 211, 213, and archive rows W7-5..W7-5d, W7-12r, W7-39, W7-47, W7-56..58, W8-8,
W13-8, W14-14.

**Wrong assumption:** a job's life is "its sched slot plus whatever its body writes", and width is a
per-sched fact.

**Single sources:**
1. **One job-state transition** on `SchedCore`, under its lock: Held → Running → Parked → Done(outcome).
   - Every event goes through it: finish, exit, fault, shutdown, shutdown_now, verdict, creator cancel.
   - It reads the run halt before it releases anything.
   - `Task` / `submit_result` read the slot's outcome. `done()` means the slot is Done.
   - It deletes `task.chz`'s `TaskState` core and the `recv`/`try_send` dance, `run_outcome`'s settle
     channel, the split release/drop policies, and `join_executor`'s Deadlocked→Cancelled remap
     (`sched.rs:5343`).
2. **The verdict is a run halt.** `QuiesceState::verdict` decides once and publishes through the same cell
   and funnel as exit and job fault (`run_exit_err`). Every party ends there, main included.
   `flag_deadlock_leaves` only records slot outcomes. This deletes the deadlock exclusion at
   `sched.rs:2561-2565` and the judge-order race.
3. **One process-wide runner budget of N** (the `RUNNERS` gate at every T). Every thread that runs Chezzi
   code holds a permit: main, drainers, helpers, Executor runners. `claim_runners` stays as per-sched
   demand only. This deletes `NestedDrainerSlot`'s second budget, the T==1-only conversions, and the
   Executor exemption.

**Grid test:**
- Jobs: state {held, running CPU, parked recv, parked guard, parked sleep, done} x event {own exit,
  foreign exit, fire-and-forget fault, handle fault, shutdown, shutdown_now, verdict judged by main /
  by a sched worker, creator cancel} x handle {submit, submit_task, submit_result} x party {spawn,
  Executor(), Executor(1)}.
  - Nothing runs after exit or a run fault.
  - Every handle settles exactly once.
  - `done()` is monotone.
  - The spawn and Executor rows agree.
- Width: {1 Executor, 2 Executors, Executor + `parallel:`, Executor + busy main, nursery in a job} x
  T in {1, 2, 4}; peak runners process-wide ≤ T.

## Family P2 — what a bracket after a path means (S1, S2, CK1)

The unfinished part of TICKET-210's one resolver. Three separate duplicates.

**Status (2026-10-07):** S1 and CK1 fixed by TICKET-222 (R1 + R2: one bracket node read with the
type grammar; `resolve_path` the one writer of a path's resolution, the call fallback deleted). S2
is open (R3).

- **S1, two type grammars for one bracket.**
  - The call form `[X](` parses `X` with `parse_type` (`try_parse_type_arg_call`, `src/parser/mod.rs:3043`).
  - With one argument and no `(`, the bracket is parsed as an expression (`parse_subscript`, `:2826`)
    and turned back into a type by `ast::index_as_type` (`src/ast/mod.rs:1209-1235`). That function knows
    only `Ident`, `Index`-over-`Ident` and `Field`-over-`Ident`, not `Map[str,int]`, a tuple, `lib.Bx[int]`
    or `fn(..)`.
  - **Single source:** the parser keeps both readings from the one type grammar (`Index.as_type`, as
    `bracket_expression` `:3095` already does for calls). `index_as_type` is deleted.
- **S2, built-in enums are not ordinary enums.**
  - `type_head` (`src/checker/setup.rs:2615`) answers from `struct_names`/`enum_names`.
  - `Option`/`Result` live instead in inline `variants_of` arms (`sig.rs:6061-6090`), a prelude mirror
    with a drift `debug_assert` (`setup.rs:1185-1222`), `builtin_ok` (`pattern.rs:4176`), `resolve_type`
    arms and the compiler's variant registry.
  - The documented `Result[int, str].Ok(5)` (a10c1043) was never tested.
  - **Single source:** register the prelude's `native enum Option`/`Result` as ordinary enums. This
    deletes the inline arms, the drift assert and `builtin_ok`.
- **CK1, two writers for one node.**
  - The call side's "DEFENSIVE FALLBACK" (`src/checker/expr.rs:649-718`) records `ModuleMember` on the
    `Field` of `(math.abs[int])`.
  - The value side (`infer_type_applied_fn_value`, `pattern.rs:2788`) then records `Fn`/`Decode` on the
    same node, and `reject_table_conflicts` raises W7-49.
  - **Single source:** a call on a non-name callee is the path value applied (TICKET-210's own rule).
    Delete the fallback block.

**History:** about 66 commits on TICKET-180/187/196/197/201/204/210/214/215.

**Grid test:** type-arg shape {`int`, `List[int]`, `Map[str,int]`, `(int,str)`, `fn(int)->int`, `lib.T`,
`lib.Bx[int]`, `Option[int]`} x head {local generic, imported, `lib.f`, `json.decode`, `math.abs`,
`Bx[..].static`, `Bx[..].inst`, `E[..].Payload`, `E[..].Nullary`, `Result`/`Option` variants} x position
{call, let, typed let, HOF arg, `(h[X])(..)`, `Head[X].m(..)`, `==`}. Extend `tests/turbofish_value_grid.rs`
and `tests/path_call_grid.rs`, which today use only `int`/`str` as type arguments and never `Option`/`Result`
as a head.

## Family P3 — what kinds of fn value exist, and what every consumer handles (CK3, A4)

- **CK3: no single memo for synthesized fns.**
  - `Vm::fn_value` keys `Proto | Native | Cffi` (`src/vm/mod.rs:895`).
  - The compiler memoizes variant fns (`variant_fns`) and param-method fns (`param_method_fns`).
  - But `compile_decode_fn_value` (`src/compiler/mod.rs:5306`) emits a new proto at every read site, so
    each read is a new object.
  - Its body also reads the reader's `json` import slot, which breaks `fn_value`'s "a synthesized proto
    reads no global" invariant.
- **A4: a hand-listed set of callable kinds per consumer.**
  - `invoke_value` handles all six kinds (`src/vm/call.rs:146`).
  - `lower_task` (`src/vm/sched.rs:5033-5093`) handles only Closure/Func/Builtin.
  - `Executor.submit` (`src/vm/netio.rs:4144`) handles only Func/Closure.
  - Patched once before by adding a `Lowered::Builtin` arm (6bbdd353).
- **Single sources:**
  - One compiler memo `synth_fn_proto(SynthFn::{Variant, ParamMethod, Decode})`, with the decode body
    reading no home global.
  - One classifier `Vm::callable(Value)`, read by call, spawn, submit and the entrypoint. Non-closure spawn
    callees cross through `to_wire`/`from_wire`, which already handle every kind.
- **Grid test:** fn kind {plain, method path, variant, `T.m`, `json.decode[T]`, closure, native, builtin,
  extern} x consumer {call, spawn callee, spawn arg, `submit`, `submit_result`, channel round trip, `==`
  (same site, two sites, two modules), repr, snapshot}, plus a completeness test that fails when a new
  callable `Obj` lacks a consumer arm.

## Family P4 — a generic fn value at a join (CK5)

- `generic_fn_value_ty` (`src/checker/pattern.rs:2342`) decides pin-or-reject at the read, from the
  expected hint only.
- The one deferral is for call arguments (`generic_fn_value_prepass`, `expr.rs:4533`).
- if/match/`??`/list/map/set/`==` joins reject before `unify_branch` (`pattern.rs:1347`) sees the sibling.
- **Single source:** one join operation that pins a deferred generic read with the determined sibling's
  type, or rejects when none is determined. The call-argument deferral becomes its special case.
- **Grid test:** join {if, elif, match, `??`, list, map, set, `==`} x order {generic first, last} x
  sibling {concrete fn, `Option[fn]`, another undetermined generic (must reject)}.

## Family W2 — where a width meets a constant (FF1)

- `check_const_fits` (`src/checker/expr.rs:4355`) is hand-paired with `assignable` at about 15 of 56 call
  sites; its doc comment says "a new site must call it too".
- Width is erased in `assignable` (`proto.rs:1195`), `compatible` (`ty.rs:571`) and `unify`
  (`mod.rs:3735`), and constness is not in the type. So:
  - a generic T binds `int` from `300` before the `int8` hint is read (`proto.rs:4706-4774`);
  - the map key store (`sig.rs:4339`) is never checked.
- There are three constant evaluators:
  - `ast::const_int_scan`: `+ - * / %` only;
  - `ast::const_float`: a literal only;
  - the peephole folder.

  The overflow lint (`pattern.rs:1591`) shares the blind spot.
- **Single source:**
  - One constant evaluator.
  - A constant is checked where a width meets it, inside the one relation every slot uses
    (`assignable`/`compatible`/`unify`), Go's untyped-constant model. Constant args seed T from the hint
    first.
  - The hand list goes.
- **Grid test:** sites taken from the `assignable`/`unify` callers x constant {literal, folded expression
  (`1 << 8`, `hi | (hi+1)`, `3e38 + 3e38`)}.

## Family A2 — one task-copy marking walk (A3)

**Status: fixed (TICKET-220, 2026-10-07).** `arg_mark_task_copy` is deleted; a copy's read rebuilds through `std.concurrency.task_copy_of` under `Route::CopyRead`.

- There are two walks that mark task copies:
  - the rebuild walk (`from_wire_memo` + `copy_mark`, `src/vm/sched.rs:4340`), which applies TICKET-190's
    generator frame mask in `rebuild_frame_slots` (`:4307`);
  - the in-place `arg_mark_task_copy` (`src/vm/mod.rs:6742`), the only `Route::CopyRead` walker, which
    marks everything in `GeneratorCore::gc_roots` and ignores `private`.
- TICKET-213 already hand-copied one rule into the second walker (`87bb7e27`, the module stop).
- **Single source:** delete `arg_mark_task_copy`. A copy's read rebuilds under `Route::CopyRead` through
  `from_wire_memo`, so every rule the rebuild has, CopyRead has.
- **Grid test:** add `task_get` and `memoize` routes to `tests/chz/spec/airlock_generator_route_test.chz`,
  expecting the `marking` row.
- Fix the stale route lists in `docs/decision-d4-airlock.md:155-160`, `crossing.rs:88-90` and
  `docs/concurrency.md:1786`.

## Isolated

- **C3:** **FIXED (TICKET-221, 2026-10-07).** Chezzi follows redirects itself, and each hop's own method
  decides its retry, as Go's. The text below is the analysis as filed.
  `std.request`'s retry decides from the original method (`src/native/request.rs:146`). Go decides
  from the failed hop's method. ureq follows redirects inside one call, so the fix needs Chezzi to follow
  redirects itself, or to learn the failed hop's method.
- **CK6:** diagnostics are printed with `eprintln!`, which panics on a closed stderr. There are 89 sites in
  `src/`. Stdout already goes through `emit_out`/`stream_halt`. **Single source:** one diagnostics writer
  that ends quietly on EPIPE, used by every site. **Fixed in place 2026-10-07:** `chezzi::outln!`/`errln!`/`out!` (`src/lib.rs`) for every CLI message in `src/main.rs` and `test_runner`'s warning line.

## Owner decisions needed

1. **Bare `Some` / `Ok` / `Err` as fn values** (`[1, 2].map(Some)`, Rust's `.map(Some)`). Registering
   `Option`/`Result` as ordinary enums makes `Option.Some` a payload-variant value; should the bare
   prelude names follow?
2. **Width = N process-wide at every T** (CHAN1), main included, as Go's `GOMAXPROCS`. TICKET-205 accepted
   the gate's cost at T=1 only. The gate at T>1 needs measuring.
3. **C3:** follow redirects in Chezzi so the failed hop decides the retry (Go), or document the divergence.
   Decided 2026-10-06: match Go (TICKET-221).
4. **Held job on exit / run fault / shutdown_now:** it never starts, and its handle settles `Err(cancelled)`
   (CPython `cancel_futures`). This is already what the docs say, so it is listed only for confirmation.

## Owner decisions (2026-10-07)

1. **Only run-wide halts reach into a CPU-bound task at a call** (TICKET-224, option c): `os.exit` and a
   fault that ends the run (a no-handle `ex.submit` job fault, an unhandled top-level fault) stop a
   loop-free recursion at its next call. A nursery's implicit cancel and `std.cancel` stay at waits and
   loop back-edges (DEC-194 unchanged), as Go (`os.Exit` immediate, `context` cooperative) and Python
   (`TaskGroup` cancels at `await`).
2. **`parallel:` stays fail-fast** (the first fault cancels its siblings, then re-raises at the block
   end), the default of Python `TaskGroup`, Trio and Go `errgroup`. A cancelled sibling stops only at a
   cut point, runs its `defer`s, and its partial state stays in its own airlock copy. "Wait for all" is
   already expressible with handle jobs (`submit_task` each, `get()` each).
3. **Future, not filed:** a `shield:` block (Trio `CancelScope(shield=True)`, `asyncio.shield`) for a
   section that must finish once started.
4. Runner width (CHAN1) is its own ticket, TICKET-230.

## Plan order (proposed)

1. **E2** (job lifecycle): C1, C1b, C2, CHAN3, CHAN4, CHAN1. The largest family; may split width into its own ticket.
2. **P2 + P3** (paths and fn values): CK1 P0, CK3 P0, S1, S2, A4.
3. **A2** (CopyRead walker): A3.
4. **W2** (width constants): FF1.
5. **P4** (join pin): CK5.
6. **Isolated:** C3 and CK6.
