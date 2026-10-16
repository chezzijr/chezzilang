# Gaps archive: checker item and sendability backlogs (2026-07-21/22) (moved verbatim from `docs/history/bug-hunts/gaps-archive.md` lines 1253-1458)

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
