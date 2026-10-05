# Root causes — bug-hunt wave 20 (JIT sweep #5, 2026-10-03)

Sweep #5 ran five hunters (airlock, cancel/faults, channels/scheduler, checker/names/generics, stdlib vs
CPython) on `main` at `cfac5b11`, with extra weight on TICKET-199..205. Every finding below was re-run
by the main loop on the release binary. **Not clean: 2 P0 and 10 P1.** The JIT entry rule's "two
consecutive clean sweeps" count stays at zero.

The pattern this wave: TICKET-204 unified the VALUE side of "what a path denotes" and left the CALL side,
the native modules and the runtime identity each with their own copy; and an Executor job builds its
inherited context through a different path than a spawned fiber, so every party-context fact
(globals, cancel parent, exit cleanup, runner permit) has a job-specific copy that disagrees.

## Findings

| ID | Sev | Repro (short) | Chezzi | Ancestor |
|---|---|---|---|---|
| A2 | P0 | global `xs := [1]`; job1; `xs.push(2)`; job2 reads `xs` | `job2 [1]` (stale) | CPython/Go `[1, 2]`; docs: a job sees globals as of its submit |
| K1 | P0 | `m := P.mk; m == P.mk`; `a := E.A; a == E.A` | `false`, `false` | CPython/Rust `true`; syntax.md §Eq (W7-54) |
| H1 | P1 | T=1: an Executor job burns CPU while main burns CPU | wall 2.1 s, user 4.1 s: two runners at T=1; seeded replay with a job 10/10 distinct | Go `GOMAXPROCS=1`: one core |
| C1 | P1 | Executor created inside a job's nursery fiber; outer `shutdown_now()` | inner job runs its full 3 s (one created in the job body is cut) | Go: the context cuts both |
| C2 | P1 | `chezzi test`: a top-level / `before_all` Executor job faults | `2 passed`, rc 0, fault dropped | `go test`: FAIL with the panic |
| A1 | P1 | owner never called `t.get()`; a copy does `t.get().push(3)` (also `memoize1`) | no fault, write lost | CPython `[1, 3]`; D4: a copy write faults, never vanishes |
| K2 | P1 | in `fn o[V]`: `h: fn(V) -> V = ident`, `xs.map(ident)` | `cannot assign fn(T) -> T to ... fn(V) -> V` | Go accepts |
| K3 | P1 | `P.get(P(1))`, `(Bx[int].get)(Bx(3))` | `'get' is an instance method; call it on a value` | CPython/Rust `1`, `3` |
| K5 | P1 | local `K := 1` shadows `struct K`; `fs[K](10)` | `'fs' takes no type arguments` | CPython `12` |
| S1 | P1 | `[-1, 2].map(math.abs)`, `g := math.abs; g(-3)` | value takes the float-only sig | CPython `[1, 2]`, `3` |
| S3 | P1 | `std.request` reusing a keep-alive connection the server closed | `io: Peer disconnected` on ~1-5% of GETs; `/loop` Err 7/12 | Go/CPython 0/40 (retry an idempotent request) |
| H2 | P2 | nursery inside a spawned task, outer body closed, 8 spawns | ~2 cores at any T (8.6 s vs flat 2.1 s at T=8) | Go nested == flat |
| C3 | P2 | `os.exit(3)` with a sibling parked in a native callback in a JOB's nursery | that sibling's `defer` is skipped (runs in the main nursery) | Chezzi's rule runs both |
| K4 | P2 | `lib.Nope[str](1)` | `method 'Nope' takes no type argument(s)` | Python: no attribute `Nope` |
| K6 | P2 | `type BI = Bx[int]; BI[str].make(..)` | "constructors are not values" | Rust E0107: alias takes 0 generic args |
| S2 | P2 | `d := json.decode[int]` | `module 'json' has no member 'decode'` + `unknown name 'int'` | Rust `parse::<i32>` as a value |

A3 duplicates K3. Repros: `~/.cache/hunt5/{air,cancel,chan,check,std}/` (`air/p/A2.chz`, `A1.chz`;
`check/k/a5.chz`, `b2.chz`, `b4.chz`, `d4.chz`, `e8.chz`, `mm/b7.chz`; `std/p/s1.chz`, `jd.chz`, `rq5.chz`
+ `srv.py`; `cancel/p/F1.chz`, `F2_test.chz`, `x2.chz`; `chan/h2.chz`, `h2seed.chz`, `w8.chz`).

## Family P1 — what a fn-like path denotes, in every position (K1-K6, S1, S2)

**Fact:** what does a path or name, with optional type arguments, denote — the same answer whether it is
called, bound, passed, parenthesised or compared.

**Deciders today (six):**
- A. Parser: `try_parse_type_arg_call` (`parser/mod.rs:3035`) turns any `name[Type](args)` into
  `Call{type_args}` without scope; `[...]` with no call stays an `Index` (`ast::type_application`).
- B. Call position: `infer_call_dispatch` (`checker/expr.rs:88`), ~15 hand-written head arms (196-560).
  `infer_static_call` refuses instance methods (`expr.rs:1331-1337`, K3); `infer_named_call` rejects type
  args by `name_is_generic` without asking whether the name is a local (`expr.rs:2139`, K5);
  `infer_method_call` checks member turbofish arity before existence (`expr.rs:3224`, K4); `json.decode`
  matched by name in call position only (`expr.rs:176-195`); `math.abs` polymorphism attached in call
  position only (`expr.rs:3263`, `:2897`).
- C. Value position: TICKET-204's `path_fn` (`checker/pattern.rs:2460`) and its helpers; `module_fn`
  sees only `certain_fn` (S1: float-only `abs`; S2: nothing for `decode`); `infer_index` falls back to a
  real index for a non-path head (K4 `Nope[int].make`), and an alias with args reaches `type_not_value`
  (K6).
- D. Type head: `type_head` (`checker/setup.rs:2713`) consults scope; the value side respects locals,
  the parser + call side do not (K5).
- E. Native metadata: `MODULE_NUMERIC_POLY` (`checker/mod.rs:2086`) is a side-set only call arms read.
- F. Runtime identity: a plain fn is one `Obj::Func` stored in its global; `MakeMethodFunc`
  (`vm/exec.rs:2683`) and `MakeFunc(variant_fn_proto)` allocate on every read, and fn `==` compares
  handles (K1).

**K2 separately:** `pin_generic_fn_value` (`checker/mod.rs:~3421`) unifies the callee's free `T` with
the caller's rigid `V`; `Ty::Param` compares by name, so it cannot tell a free param from an in-scope
rigid one (the same program with the caller param named `T` works).

**Grid:** path {plain fn, generic fn, static method, method via type, payload variant, native module fn,
std generic native, imported/qualified} x position {call, let, typed let, HOF arg, parenthesised call,
`==`, pinned to a caller's abstract param}. Broken: method-via-type call and paren-call (K3); `==` for
every allocated path value (K1); native fns as values (S1, S2); every "abstract V" pin (K2); index-call
through a local named like a type (K5); two wrong messages (K4, K6).

**History:** ~42 ticket commits on this question (TICKET-180, 187, 196, 197, 201, 204; W7-54) plus
~10 on the pin rule (W8-44 x3).

**Wrong assumption:** call heads and value paths are different questions. TICKET-204 unified the value
side only.

**Single source:** one scope-aware resolver `resolve_path(expr, type_args) -> PathFn` read by both call
dispatch (a call = the value's type + apply) and value reads; the parser's `[types](args)` stays a hint
the resolver may reread as index-then-call when the head is a local. Natives join through their
`ModuleSig` entries carrying the polymorphic / decode kinds as real signatures, not side-tables. Free
callee params are made fresh before `unify`, so a caller's rigid param pins like a concrete type. A
path value gets one canonical `Obj::Func` per (type, method) or (enum, variant), memoised like a
top-level fn's global.

## Family E1 — what a new party inherits (A2, C1, C3, H1)

**Fact:** what context a new party inherits from whoever starts it: the globals view, the cancel
parent, whether exit waits for its cleanup, and the runner permit.

| inherited | spawned fiber | Executor job (and its nursery fibers) |
|---|---|---|
| globals view | the live parent heap | `ensure_snapshot` returns a memo cached at the first submit (`vm/sched.rs:5167`, `:5675`); only a slot write or a nursery open drops it — `submit` does not (A2) |
| cancel parent | `scope_ancestors()` chain (`sched.rs:1536`) | `creator_cancel`, set only `if self.eager_core.is_some()` (`vm/exec.rs:3063`); a job's nursery shells have `eager_core: None` (`exec.rs:174`), so an Executor made there inherits nothing (C1) |
| exit cleanup | the owner's join runs the defers | `drain_live_executors_from` returns at once on `pending_exit` (`vm/netio.rs:4284`); nobody joins a job's nursery, and a callback-parked fiber wakes only on the 5 ms poll (C3) |
| runner permit | the scheduler gate | pool threads are born ungated (`vm/pool.rs:66`); `width::acquire()` no-ops unless gated (`width.rs:78`), and main never converts: T=1 runs 2 runners (H1) |

**Grid:** item {globals, cancel parent, exit defer, permit} x party {spawned fiber, nursery child,
Executor job body, job's nursery fiber, executor created in a job body, executor created in a job's
nursery fiber, callback-parked fiber in each}. Broken: the four cells above. The spawn variant of A2
is correct (`s1 [1] / s2 [1, 2]`), so the docs' "in-place caveat" (`docs/concurrency.md` ~137) is
stale for fibers.

**History:** 9 tickets each patched one row for one party: W6-2, W7-39, W7-47, W7-57, W10-12/TICKET-067,
TICKET-188, 195, 200, 205 (42 commits).

**Wrong assumption:** "a job is a fiber, except where I patch it", with job-ness keyed on the wrong
signal (`eager_core` on the current VM; "the nursery open is the last moment globals change"; "a
thread that needs the gate is already gated"; "the owner's join runs the defers").

**Single source:** one constructor, `Vm::inherit_party(kind) -> PartyCtx`, called by `spawn_shell`,
`prepare_eager_job` and `Op::NewExecutor`. It decides once: a fresh globals view (drop a non-reusable
memo at every spawn door, submit included); the full ancestor chain (job-ness read from the chain);
a gated birth for every thread that runs Chezzi code; and whether exit joins the party until its
defers run (bounded by the halt).

## Family W1 — how many runners serve a scope family (H2)

**FIXED (TICKET-211, 2026-10-05).** One claim, `MnSched::claim_runners` over
`SchedCore::runner_wids`, runs at `inject_or_extend`, `Executor.submit` and a body block; the three
farm functions, the blocked-body latch and the wid ranges are deleted. Runner threads are reused
across nurseries through `src/vm/runner_cache.rs`. Grid test:
`vm::tests::runner_threads_reach_the_worker_count_in_every_nesting_shape`; base vs fixed in
`docs/benchmarks.md` §TICKET-211. The text below is the analysis as filed.

**Fact:** how many workers serve a scheduler's runnable work. Decided once, at the outer
`close_body` (`farm_outermost_eager_helpers`, `vm/sched.rs:1367`, guard `outstanding_tasks() < 2`), and
by TICKET-159's hook only while the body is open and blocked (`farm_blocked_body_helpers`, `:1320`).
Since TICKET-131/132 a nursery inside a fiber registers on the outer sched (`activate_fiber_owned_nursery`,
`:1125`) and adds tasks after close; nothing recounts. Measured (8 tasks): nested closed 2.89-3.09 s at
T=2/4/8 (2 cores); flat 0.84 s at T=8.

**Wrong assumption:** a sched's task count is final at `close_body`.

**Single source:** recruit at `inject_or_extend` (`vm/mod.rs:3270`), the one place work becomes
runnable: whenever runnable work exceeds active runners, up to `worker_count()`, with raw threads from
`NestedDrainerSlot` (pool helpers hung tests, `sched.rs:1270`), keeping T=1 at one runner (W8-8) and not
counting the inline joiner twice. The close-time farm and TICKET-159's hook fold into it.

## Folded into TICKET-207 (Executor fault model, owner decision 2026-10-03)

- **C2**: `Vm::reap_after_tests` discards `drain_live_executors`' result (`vm/exec.rs:969`); under the
  new rule a fire-and-forget job fault ends the test run.
- **A1**: a copy's write to an unobserved `get()` result is lost; `Task.get()` is being rewritten to
  `Result[T]` in 207.

## Isolated

- **S3** (P1): `std.request` must retry an idempotent request once when a reused keep-alive connection
  turns out closed before any response byte (Go's rule), one site in the request client.

## Owner decisions (2026-10-04)

1. **Executor is rebuilt on the concurrency design.** A task started by `ex.submit` and a task started
   by `spawn` are one kind of task with the same rules: globals copied at start (like `fork`), a write
   to that copy faults (D4), sharing only through `Shared`/`Atomic`/`Channel`, cancel inherited from
   the parent chain, one runner at a time under the gate. The Executor only manages lifetime and adds
   pool features on top: at most N at once, handles, not scoped to a block, `shutdown()` instead of the
   block end. This replaces family E1's "one constructor" fix and absorbs TICKET-207 (`Task.get()`
   returns `Result[T]`; a fire-and-forget job fault ends the run at once; A1; C2) and TICKET-206's
   wait 4 (job start). Open idea, not decided: a spawn that returns a handle (`t := spawn foo()`).
2. **`os.exit` runs no `defer` anywhere** (Go `os.Exit`, Python `os._exit`): no sibling cleanup.
   Buffered stdout is still flushed. C3 disappears; `docs/concurrency.md` (~1206, "a defer is never
   itself cancelled ... every registered defer of a cancelled task runs") changes for the exit case.

## Owner decisions (2026-10-05)

1. **An unpinned generic path value is rejected, natives included.** `g := math.abs` is an error with
   the hint `math.abs[int]` (the DEC-204 rule); `[-1, 2].map(math.abs)` stays accepted, pinned by the
   expected type; `math.abs[int]` is legal.
2. **`Num` protocol.** A reserved structural protocol satisfied by `int` and `float`; `math.abs`/`math.sign` become `[T: Num](x: T) -> T` and the call-only
   `MODULE_NUMERIC_POLY` side-set goes. A `T: Num` body gets only the operators whose result type is
   `T` for both `int` and `float`.
3. **The seed is a fuzzer first.** Byte-for-byte T=1 replay is not required for every wait kind. Waits
   that replay exactly stay exact; the `Shared` guard wait and the Executor wait stay rate fixtures in
   `tests/sched_seed/open/` (W15-10). No guard hand-off queue; a replay fix ships only if it costs
   nothing unseeded.
4. **`newtype` is removed** (TICKET-216). It inherited operators for numeric underlyings only and
   nothing else; a one-field `struct` plus protocol methods replaces it (Rust's tuple struct). `Num`
   (decision 2) is satisfied by `int` and `float` only.

## Plan order

1. **Executor rebuild** (A2 P0, C1, C3, H1; TICKET-207's fault rules, A1, C2; TICKET-206 wait 4) — TICKET-208. TICKET-207 closed into it.
   Status (2026-10-04): TICKET-208 landed the engine. TICKET-213 landed A1 (a copy's write to an unread `Task.get()` / `memoize1` result faults), `Executor(n)` and the exit rule (`os.exit` runs no `defer`), which deletes C3. Seeded replay of the Executor stays a measured rate (W15-10).
2. **P1** (K2, K3, K4, K5, K6) — TICKET-210. Split off: S1+S2 (native fns as values, `Num`) — TICKET-214; K1 P0 (path-value identity) — TICKET-215.
   Status (2026-10-05): K2-K6 landed in TICKET-210, plus a bound's instance method through its type parameter (`T.get(v)`); grid `tests/path_call_grid.rs`. S1+S2 and K1 are the split-off tickets.
   Status (2026-10-05): S1 and S2 landed in TICKET-214 (`Num`, sealed to int/float; `math.abs`/`math.sign` are `[T: Num]`; `json.decode[T]` is a value).
3. **W1** (H2) — TICKET-211; after TICKET-208 if they share scheduler files.
4. **S3** — TICKET-212. Status (2026-10-05): landed; `std.request` retries a request Go would retry (`isReplayable`), once, on fresh connections.
5. **W15-10 remainder** (in-place nursery join only, if free unseeded; the `Shared` guard wait stays a rate, decision 3 above) — TICKET-209; TICKET-206 landed wait 1.
   Status (2026-10-05): TICKET-209 shipped no engine change. Both waits stay rate fixtures in `tests/sched_seed/open/` and W15-10 stays open: the exact fixes cost about 2x (join) and 7.6x (guard) unseeded, and the owner ruled that a wait becomes exact only when that is free.
