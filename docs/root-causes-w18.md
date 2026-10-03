# Root causes — bug-hunt wave 18 (JIT sweep #3, 2026-10-01)

Sweep #3 ran five hunters (airlock/generators, faults/cancel/control flow, channels/Shared/Executor,
checker/names/modules, stdlib vs CPython) on `main` at `23389aef`, with extra weight on the code
TICKET-184..193 merged. Every finding below was re-run by the main loop on the release binary.
**Not clean: 4 P0 and 5 P1.** The JIT entry rule's "two consecutive clean sweeps" count stays at zero.

The pattern this wave: every P0/P1 sits in a family wave 17 fixed, at a boundary the wave-17 fix did
not reach — the export side of a per-module record, a second substitution pass, a second delivery
path that skipped the first one's semantics, the first stage of a two-stage wait. The grids were
complete inside the code they touched and silent about the code next to it.

## Findings

| ID | Sev | Repro (short) | Chezzi | Ancestor |
|---|---|---|---|---|
| K5x | P0 | lib: `fn f(a, b)` then `f := fn(b, a)`; main: `lib.f(a=1, b=2)`, `import f from lib` | binds the OLD fn's labels/defaults/variadic, runs the new body: `21`, `f2 a=2 b=1`, a check-OK `expects 1 argument(s), got 2` | CPython `12` / `a=1 b=2` |
| K6x | P2 | lib `Y: const int = 5`; main `import Y from lib; Y := 7` | accepted | rejected for a local const (syntax.md "No laundering") |
| N1 | P0 | `struct Box[T]: fn pair[U](self, u: U) -> (T, U)`; `fn go2[T, U](b: Box[U], t: T) -> (T, T): return b.pair(t)` | check OK, wrong type, runtime `int has no method 'upper'`; the correct `-> (U, T)` is rejected | Rust E0308 / accepts the correct one |
| N2 | P1 | `fn h[U, T](xs: List[U], f: fn(U) -> T) -> List[T]: return xs.map(f)` | `expected fn(U) -> U` (false reject) | Rust/Go accept |
| N3 | P1 | `fn v(a: int, b: int = 10, ...rest: int)`; `w := v; w(1)` | `missing required argument 'b'` (direct `v(1)` is 110) | CPython accepts; syntax.md promises defaults through a value alias |
| N4 | P1 | `import idt from lib; i := idt[int]` / `lib.idt[str]` | `'idt' is generic and T is not determined here` (same-module `idt[int]` works) | Go `lib.Idt[int]` accepted |
| C1 | P0 | owner in `ex.shutdown()` (Executor made OUTSIDE the nursery) cut by a child's panic | the outside Executor's running job is cancelled: `job done? None` | CPython: the job finishes; concurrency.md:1133 says it is not cancelled |
| C2 | P1 | `parallel:` child panics `boom`; the body's `defer: ch.recv()` can never complete | `recv ... deadlock`, `boom` lost (also from a fn owner, a spawned owner, a job owner) | Go `panic: boom`; CPython reports boom |
| C4 | P2 | same, with `defer: panic("cleanup failed")` | `recover:` returns `cleanup failed` | concurrency.md: a cancelled party's defer fault ranks below the root cause |
| C5 | P2 | cut owner's stack trace | shows the owner's `waiter`/`owner` frames for the child's `boom` | CPython shows the child's `a` -> `b` |
| C3 | P1 | Executor job faults (index error); main blocks on the channel the job would have sent | `recv ... deadlock`, the job's fault never printed | Go prints the index panic |
| X1 | P0 | a job returns, then `shutdown_now()`; `submit_result`/`submit_outcome` | value lost 9/10 at T=4/0, or BOTH `Some(7)` and `cancelled`; a plain user `out.send(v)` into a buffer with room is lost too | CPython and Go keep a finished job's result |
| G1 | P2 | two `Shared` boxes updated in a loop, `CHEZZI_SCHED_SEED=1` | 8.2 s at T=1 (0.04 s unseeded) | — (and the same convoy, rarely, unseeded) |
| S1 | P2 | `json.decode[Cfg]('{"name":"x"}')` with `retries: int = 3` | `Err("decode: missing key 'retries'")` | Go keeps the default; serde `#[serde(default)]` |
| S2 | P2 | `"{-0.0001:z.1f}"` | `unknown type char 'z'` | CPython 3.11+: `0.0` |
| N5 | P2 | operator protocol with a misnamed param (`fn add(self, o)`) | `cannot apply + to V and V`, no mention of the parameter name; `index`/`contains`/`slice` accept the same misname directly | — |

Clean this sweep (the hunters' "probed and clean" lists): the TICKET-189..192 airlock, generator, Task
and shared-map code over ~60 shapes; the TICKET-185 channel protocol under high-iteration counters at
T=1/2/4/0 plus seeds; TICKET-184 control flow; module `return` and read-before-init; std vs CPython over
thousands of generated cases (slicing, int parsing, float format, strings, json, csv, path).

## Family D2 — the module export (K5x, K6x)

**Status:** fixed by TICKET-196 (ModuleSig::members, one export record per slot).

**Fact:** what module slot X holds once its module has finished initialising (labels, defaults,
variadic, arity, const-ness). An importer always runs after every declaration in the module.

**Deciders.** `capture_sig` (`src/checker/setup.rs:2314-2352`) exports each `fn` into
`sig.functions` and each let into `sig.values`; a name declared both ways lands in both maps. Every
importer reads `functions` first: from-import `setup.rs:2254`, qualified call `expr.rs:3334-3341`,
qualified value `pattern.rs:4148-4163`, plus `pattern.rs:2468/4133`, `expr.rs:3037/2153`,
`sig.rs:615/1561`, `fn_writes.rs:935/974`. `bind_call` then writes the old fn's fills into
`call_plans`, and the compiler emits them (`compiler/mod.rs:4321/5039`). A from-import is recorded as a
plain `DeclKind::Import` (`globals.rs:125-128`), so an imported const is not const in the importer.

**Wrong assumption:** "module X exports fn f" means "slot X.f holds that fn" — that fn decls and lets
are disjoint namespaces. TICKET-186 removed that premise inside a module (`GlobalBinding`) but never
exported the record; `capture_sig` was not touched.

**Single source:** export the defining module's `GlobalBinding` verdict, one `ModuleSig` entry per
name: the slot's final type, `certain_fn: Option<FnSig>` (set only when declared once as a fn/extern/
native and never written), and `is_const`. Every importer path reads that one entry; a redeclared slot
exports no labels/defaults/variadic, so keyword/defaulted/variadic calls through it are denied as
`KwDeny::Redeclared` already is. A from-import records its const-ness, so the existing const-mix rule
rejects `import Y from lib; Y := 7`.

**Grid:** read path {qualified call, from-import call, qualified value, from-import value, defer/spawn
call forms, generic/turbofish member} x declaration shape {fn only, fn then `:=`, `:=` then fn, const,
extern/native} x fact {labels, defaults, variadic, arity, const, write summary, divergence}.

## Family F2 — type-parameter identity and fn-value shape (N1, N2, N3, N4, N5)

**Status:** fixed by TICKET-197. Four single sources: `instantiate_method` applies one combined map
to a method signature (N1/N2); a fn value carries its declaration's slots in `FnLabels.slots`, and
`kw_certain` records the keys an alias's certainty was read from (N3); `infer_index` asks
`generic_fn_value_sig` and the compiler erases a turbofish on `Resolution::Fn` for an `Ident` or a
`Field` head (N4); `Checker::hook_impl` asks `param_name_mismatch`, and one `name_mismatch_text`
renders the reason (N5).

**N1/N2 fact:** which declaration a `Ty::Param` in a method signature refers to. There is one `subst`
(`src/checker/mod.rs:3389`), keyed on the bare name, but every method-call path applies TWO maps in
sequence: the receiver map (`struct_param_map`, `expr.rs:3551/3557`, ~3743, ~3843, ~4000, 3156, 4629),
then `infer_generic_method` (`proto.rs:5243`) unifies and substitutes the method's own map
(`proto.rs:5331/5369`). After pass 1 the caller's `U` and the method's `[U]` are the same
`Ty::Param("U")`. N2 is the same capture against the prelude's `map[U]` (`std/prelude.chz:77`).
**Wrong assumption:** "pass 1's output contains no names pass 2 rewrites" — false once the caller is
generic. **Single source:** one combined map keyed on the declaration's parameters, applied once to
the declared signature (or fresh-named method type parameters at registration). Grid: receiver kind
{struct, enum, newtype, native List/Map/handles, bounded param, protocol} x method-param binding {arg,
turbofish, receiver, hint} x name relation {disjoint, equal, swapped} x position {arg, return}.

**N3 fact:** which slots of a fn value the callee fills. `FnSig.slots` (`sig.rs:205`) is right for a
direct call; a fn value carries `FnLabels { names, min, variadic }` (`ty.rs:239`), and `min` comes from
`ast::min_callable_params` (`src/ast/mod.rs:761-764`), which returns `params.len()` whenever a variadic
exists, so `callable_slots` (`expr.rs:5515`) marks no default. The nested-fn and generic value sites
(`sig.rs:3046/3062`, `pattern.rs:2503`) also drop `variadic`. **Wrong assumption:** "omittable slots are
a trailing run one number describes." **Single source:** a fn value carries the declaration's
`Vec<SlotSpec>`; `callable_slots` reads it.

**N4 fact:** is this head a generic fn, and which one. `infer_index` (`pattern.rs:4189-4240`) handles
`f[int]` only for `Ident` in `local_fn_names` (`setup.rs:3807`, same-module only); TICKET-187's
`generic_fn_value_sig` is not consulted, and the compiler erase (`compiler/mod.rs:3531-3543`) handles
only an `Ident`. **Single source:** route `infer_index` through `generic_fn_value_sig`, record
`Resolution::Fn` for a `Field` head, erase on the resolution for both shapes.

**N5** (P2): the operator-protocol rejection does not say which parameter name is wrong, and the
`index`/`contains`/`slice` direct hooks skip the name rule the protocol bound enforces. Folded into
this family's ticket: the hook-dispatch sites read the same `param_name_mismatch`.

## Family B2 — whose fault is being unwound (C1, C2, C3, C4, C5)

**Status (2026-10-02): fixed by TICKET-195.** `Vm::cut` (`block::Cut`: `Cancelled` /
`Delivered { floor }`) is the one record of "own fault or cut", replacing the `cancelled` latch and
`owner_fault_floor`; `Vm::adopt_child_fault` is its only `Delivered` writer and installs the faulting
party's trace (carried in `TaskOutcome::Fault` / `Halt::ChildFault`). `Vm::unwind_result` is the one
defer ranking for an unwind with a cause (the `unwrap_or` rankings are gone). `JoinBail` trips
`core.cancel` only for a stopped joiner (`--timeout`, the join verdict), never a cut one.
`QuiesceState::unjoined_job_fault` derives the unjoined job fault from the executor slots (no cell),
read at `on_step_fault`'s fatal funnel. `Vm::finish_run` is the one driver end and drains on a verdict.
Grid: `tests/cut_cause_grid.rs`. Deviation: no separate "unjoined job fault" cell (it would be a copy).

**Fact:** a party's unwind is either its OWN fault or a CUT (it is stopping because of someone else:
cancel, a child's fault, a job's fault), and the cause decides ranking, swallowing, the trace and what
else is stopped.

**Deciders** (the investigator's table): `halt_of` (`block.rs:296`); `deliver_halt`
(`exec.rs:1946`: `Cancelled` latches `self.cancelled`, `ChildFault` only sets `owner_fault_floor` and
returns the child's error); `on_step_fault` (`exec.rs:1573-1612`: non-cancelled goes to
`unwind_deferred`, where any defer fault replaces the original, `stmt.rs:173`); `unwind_cancelled`
(`stmt.rs:124-145`); `classify_mn_outcome` (`sched.rs:2775`: the stuck-cleanup swallow, only while
`cancelled`); `reduce_task_slots` (`sched.rs:2835-2960`); `block_halt_check` (`netio.rs:2373-2425`);
`Quiesce::verdict` (`quiesce.rs:403-433`); `join_eager_jobs_in_place` (`sched.rs:5422-5454`, every bail
trips `core.cancel` at `:5532`); the driver (`mod.rs:6616/6753/6881/7083`: `run().and_then(drain_live_executors)`
skips the drain when `run()` errs).

**Wrong assumptions:** (a) "the fault being unwound was raised on this stack by this party" —
TICKET-188 delivers a child's fault through the own-fault path, so C2/C4 rank the owner's cleanup
fault above `boom` and C5 captures the owner's trace; (b) "the joiner stopped waiting" means "the run
is over" — the W7-60 trip at `sched.rs:5532` was written for `--timeout`, and TICKET-188 added a bail
where only the joiner leaves (C1); (c) a recorded job fault is only read at a join, so main's deadlock
verdict outranks it and the drain is skipped (C3).

**Single source:** one `Cut { cause }` unwind mode set by `deliver_halt` for every non-own halt
(cancel, child fault), read by the defer ranking, the stuck-cleanup swallow and the trace (the child's
trace travels in the halt); the `core.cancel` trip keyed on the halt kind (timeout/hard halt only);
one run-wide "unjoined job fault" cell beside `quiesce.pending()`, read by `block_halt_check` and the
verdict before `deadlock`, and the driver drains executors when `run()` ends in a verdict. Grid: halt
cause {timeout, cancel, exit, child fault, job fault, deadlock} x party {main body, fn owner, spawned
task, job, nested owner, owner in `shutdown()`} x cleanup {none, stuck defer, faulting defer} ->
{reported fault, trace frames, which work is stopped}.

## Family A2 — what may stop a party mid-operation (X1, G1)

**Status (2026-10-01): fixed by TICKET-194.** `Vm::wait_halt` (`block.rs`) is the one cancellation
check, made only on an op's would-wait path; `ChannelCore::recv_ready`, `Vm::send_settled`,
`Vm::reentered` and `guard_free_then_take` are the other single sources. Grids:
`tests/cancellation_point_grid.rs`, `tests/shared_update_contention.rs::seeded_two_box_update_is_fast_at_every_worker_count`.

**X1 fact:** is an op that does not wait a cancellation point. Today the rule is "is the channel
bounded": `chan_send_step` calls `take_halt` (`netio.rs:1774`) BEFORE `send_commit`, even with room;
an unbounded send has no checkpoint; a recv with a queued value checks cancel first (`netio.rs:1987`).
So a job that already returned is cancelled at its result `send` in `std/concurrency.chz:113-127`, and a
plain user send is lost the same way (`e11`). Go: a send with room never consults the context; only a
blocking `select` races `ctx.Done()`.

**G1 fact:** the order of width permit and update guard. TICKET-193 fixed the post-budget loop
(`guard_wait_block`) but the first stage (`take_update_guard`, `netio.rs:261-266`) still waits up to
`GUARD_DEMOTE_BUDGET` for the guard while holding its permit; when the holder is preempted inside its
closure (`slice_end_in_place` in `src/vm/sched.rs`, renamed by TICKET-205; seeded mode forces it on ~25% of updates) the waiter
sits out the full 5 ms. TICKET-193's own Decisions named this remaining wait and judged it bounded;
seeded mode measures it at 1.33 ms per update.

**Wrong assumption:** "an op's cancellation/blocking behaviour can be decided per call site." The
single source is the blocking table (`block.rs`): a party may only be cut, or give up its permit, at
the point where it is ABOUT to wait. **Single source:** `take_halt` runs only on the would-wait path
(after `send_commit` returns `Offered`/`Full`, after a recv finds nothing); `take_update_guard` uses the
same await-free -> permit -> zero-budget order as `guard_wait_block` from its first attempt. Grid: op
{send cap 0/1/unbounded, recv, try ops, wait:, update/write} x state {ready, would wait} x halt {none,
cancel, shutdown_now, child fault} -> {completed, cut}; plus the guard grid under seeds.

## Isolated (fix in place)

- S1 `json.decode[T]`: a missing key whose struct field has a default takes the default (owner decision below). Not a one-site fix: field defaults are lowered at each constructor call (inline or a `$def$` provider), which the decoder cannot reach; filed as its own ticket. Fixed by TICKET-198: the descriptor carries each field's constructor fill.
- S2 the `z` format option (CPython 3.11): coerce negative zero to zero after rounding. **Fixed in place 2026-10-01** (`src/fmtspec.rs`, `tests/chz/spec/format_spec_zero_coerce_test.chz`, byte-identical to CPython on 13 cases).

## Owner decisions (2026-10-01)

1. **Automatic cancellation lands only where a task is about to WAIT, or at a loop back-edge**
   (option A). An op that does not wait (a `send` into a channel with room, a `recv` with a value
   ready, a `try_*` op) is never a cancellation point; the value is taken or delivered and the task is
   cut at its next real wait or back-edge. A function that has returned can therefore never lose its
   result to a cancel. `std.cancel` tokens stay the explicit, user-chosen mechanism (Go's
   `ctx.Done()`). Considered and rejected: B (waits only, asyncio — a CPU-bound sibling would run to
   its end before the group finishes) and C (Go-style, no automatic cancel — every sibling failure
   hangs the group unless each task checks a token; contradicts the documented structured
   concurrency).
2. **`json.decode[T]`: a missing key whose struct field has a default takes the default** (Go keeps
   the preset value; serde `#[serde(default)]`). A missing key with no default stays an error.
3. **The `z` format option is supported** (CPython 3.11+: negative zero after rounding prints as zero).

## Plan order

1. **A2** (X1 + G1): a P0 lost value in user code and the stdlib; small, blocking-table owned.
2. **B2** (C1-C5): builds on A2's cut points.
3. **D2** (K5x, K6x): checker export, independent of 1-2.
4. **F2** (N1-N5): checker names; after D2 (same files).
5. S1, S2 in place.

## Lessons (for `docs/lessons.md` §1)

- **A single source must reach every boundary its fact crosses.** TICKET-186 built one record per
  module slot and left the export (`capture_sig`) reading the old per-kind maps; TICKET-187 unified
  generic fn values and left the `f[int]` read on `local_fn_names`. When the grid is drawn, add the
  boundary axis: same module vs another module, value vs call, first stage vs retry.
- **A new delivery path inherits the semantics of the cause, not of the path.** TICKET-188 delivered a
  child's fault through the own-fault path and lost every rule keyed on "cancelled". Ask what the
  receiving party IS (cut or faulting) before choosing the funnel.
- **A "remaining, bounded" exception in a Decisions section is a cell to measure, not to accept.**
  TICKET-193 named the first-stage wait and called it bounded; seeded mode makes it 25% of updates.
