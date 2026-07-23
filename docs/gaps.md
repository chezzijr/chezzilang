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
walks EVERY module global once at the first `spawn`, reached or not, so eager-faulting there aborts any
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
- **Host handles** (Module/Native/Cffi in a value) — fundamental, stays rejected. Correct.
- **Multi-frame / pending-`defer` suspended generators** — checker-UNREACHABLE (item 3 arms a/c); no valid
  program constructs them. The rejects are defensive guards, not a user-visible limit — nothing to build.

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
- [N5](#n5-a-genuine-deadlock-tears-tasks-down-without-running-their-defers--open) — a *genuine* deadlock
  still skips defers (both engines agree, so parity holds — fixing one alone would diverge).
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
real runner. Missing: **test filtering** (`chezzi test` rejects *every* flag, so on a big suite it's all
or nothing — a ~20-line change and the best ratio in this file), fixtures/setup-teardown, coverage,
benchmarks, `assert_eq` with a diff, parallel execution, machine-readable output (`go test -json`).

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
`--errors=json` works for `check` and `run`; exit codes are correct and deliberate (type error → 1,
fault → 1, `os.exit(n)` honored, stdout write failure → 1). Missing only machine-readable *test* output.

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
the D5 `is_blocking` invariant and two-engine parity are unaffected. Verified: pre-fix a guaranteed-drop
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

### N5. A **genuine** deadlock tears tasks down without running their `defer`s — open
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
`docs/concurrency.md`.

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

### N5 status after the N6 fix — still open, deliberately UNTOUCHED
A **genuine** deadlock (every fiber parked, nothing cancelled, nothing able to arrive) still tears the
parked fibers down without running their `defer`s — on **both** engines. Serial reports it from
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

**New v1 limit it introduces:** `read_line`/`input` are deliberately outside `is_blocking` (the off-heap
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

### 5. Never-hunted surfaces (the two biggest remaining pre-JIT risks)
Five hunt waves have now swept the typed feature surface, the stdlib, concurrency, and the front-end.
**Two surfaces have never been audited at all**, and they are the memory-fragile ones:
- **GC + `unsafe` under Miri / ASan / TSan** — Tier-1 lever #3 in [`bug-discovery.md`](bug-discovery.md),
  still **unbuilt**. The GC and the OS-thread engine are the most `unsafe`-dense code in the repo.
- **FFI** — zero adversarial coverage. Precedent exists: a libffi `Cif` heap-pin bug already caused a
  **SIGSEGV** (FFI UB is layout-dependent, so it is invisible to the value-level oracles).

Neither is reachable by the panic-fuzzer, the CPython differential, the DSA judge, or two-engine parity
— all four are *value*-level oracles and cannot see UB or a data race. **This is where I would look next
before freezing.**

## Dependency versions (as of 2026-07-07)
All four are **major (semver-incompatible)** bumps — cargo shows them but won't auto-take. `cargo audit`
(2026-07-07, 152 deps) = **0 vulnerabilities, 0 warnings** → no security driver; do NOT bump
speculatively during the perf milestone.
- **libffi** 3→5 — **do not** bump speculatively (FFI UB is layout-dependent; the Cif heap-pin caused a
  SIGSEGV before). Highest risk, ~zero payoff.
- **ureq** 2→3 — a real API rewrite of `std.request`; do as its own task when 2.x nears EOL, with
  request tests + `--parallel` verify.
- **socket2** 0.5→0.6, **libloading** 0.8→0.9 — skip until a needed feature forces it.
