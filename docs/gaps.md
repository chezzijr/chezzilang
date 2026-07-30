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
three disclosed residuals `W6-9r` / `W6-10s` / `W6-10r` — see the index below. Read its session log
before touching `io`/`process`/FFI/`RwShared`/module-snapshot code. Its meta-finding — **5 of 6 P0s are "a
fix applied to SOME arms of an N-way set"** — is the highest-yield remaining lever. **Wave 7
(2026-07-28)** is running against exactly that lever; `W7-3` (a `recover:` inside a cancelled task's
`defer` was bypassed) and `W7-4` (two sibling closures over one captured local got separate cells across
the airlock) were both instances of it and are **fixed** — session logs at the end of this file.

## OPEN ITEMS — the whole backlog at a glance (updated 2026-07-29)

Everything still open, roughly by severity. **No memory-unsafety is left in the ledger** — W6-8, the
last one, was fixed 2026-07-27. Anything NOT listed here is either fixed or a safe-direction
observation. **Wave 7 batch A (2026-07-28) adds no row** — its three host-boundary findings
(`W7-1`/`W7-6`/`W7-7`) all landed FIXED; see its session log. Its deliberately-deferred sibling —
the **lossy path DECODE** in `fs.list_dir`/`walk`/`glob`/`canonicalize` and `os.getcwd` — was filed as
**W7-8** and is **FIXED 2026-07-31** (the `PathLike` protocol + `path.Path` type; see its session-log
section). The lossy-byte family now has **no unswept member**: B1, R1, W6-4, W6-9, W6-14 and W7-8 are
all closed. (`argv`/`env` remain a deliberately lossy surface — see `docs/stdlib.md`.)
**Keep this table in sync when a section is retired** — the
reason it exists is that "which of these is still open?" previously required reading 1400 lines of
chronological log.

| item | gaps.md | what | why it is still open |
|---|---|---|---|
| `min`/`max` → `Option` | `:1392` | `List.min`/`max`/`min_by`/`max_by` fault on empty while `first`/`last`/`pop` return `Option[T]` | Breaking surface change: 23 call sites + docs + examples. Own milestone |
| `List[Any]` widening | `:1309` | `List[Any] = [1, 3.0]` silently widens the int to `1.0` | Deferred pre-freeze (wave 4) |
| **N10** | `:3158` | A `wait:` timer arm makes `--serial` inline-sleep instead of yielding to a runnable sibling (serial ≠ M:N) | Deliberate pre-freeze known-limit; fix is folded into the post-freeze serial-engine removal |
| **W6-10s** | `:1179` | `--max-heap` residual **sampling** escapes left after the byte-aware pacing fix | Pacing samples the cap on charged off-heap bytes, but only for stores routed through `to_wire_crossable` and only per heap. Still not sampled: the documented inline-scalar loop (`future.md §1b` — no `Obj`s, no wire bytes), the by-hand airlock paths (spawn args, closure captures, `Executor.submit`), and a heap that HOLDS a huge core without storing to it |
| **W6-9r** | `:1303` | Parity-oracle residual left by the `W6-9b` fix: ~31 hand-rolled `run_file_p` + `run_file` cross-engine compares in `parity_tests.rs` still diff LOSSILY-DECODED strings, and `parity_entry_cfg_lines` compares stdout as an order-insensitive line multiset | The three SHARED comparators were fixed at the helper level (0 call sites touched); converting the hand-rolled ones means rewriting ~31 call sites. UTF-8-only today, so nothing is failing — but a new byte-emitting test added at one of those sites inherits the blindness. Use `vm::run_file_bytes` there |
| **W6-10r** | `:1216` | `--max-heap` residual: a payload reachable ONLY through a **nested** core (a `Channel` inside a `Shared`, once the nested core's last `Obj` alias slot is swept) is counted nowhere | Left open by the W6-10 fix on purpose. `live_bytes` reaches a core's bytes through its `Obj::*` alias slot; a nested core has none. Closing it needs cross-core byte recursion with `Arc` de-dup — narrow trigger, not worth the machinery yet |
| protocol embeds | `:1505` | A protocol-embedded method isn't callable through the interface value (`p: Person` can't call embedded `name()`) despite `spec.md:973` "flattened at bound sites" | Filed as a safe-direction observation in wave 3; never triaged — doc and behavior contradict each other either way |
| **W7-5** | `:3800` | The M:N `Executor` drain does not abort the remaining jobs after a faulting job (serial does), and `submit_result` discards the result of a job it ran | **Needs its own milestone — two fix attempts were rejected.** Sequential drain is correct but costs 4× (0.30s → 1.20s on 4 overlapping jobs); "run all" removes the drain's per-drain cancel flag, which is the ONLY kill switch — it breaks `os.exit` hard-halt, lets a faulting job leave a runaway sibling unkillable, defeats dead-stdout promptness, and creates a NEW serial≠M:N line-set divergence via `reduce_task_slots`' non-lowest-index fault flush |
| **W7-5b** | `:3800` | An `Executor` created INSIDE an M:N task is silently discarded — its jobs never run, never reap, no fault | Found while prosecuting the W7-5 fix. It registers in the throwaway worker `Vm.executors`, which `run_outcome`/`into_fiber` drop; `drain_live_executors` only snapshots the PARENT `Vm`. Arguably worse than W7-5 itself; belongs to the same Executor milestone |
| **W7-5c** | `:3800` | `reduce_task_slots` flushes a faulting task's buffered output only `if first_fault.is_none()` (`sched.rs:1688`), so a second faulting task's stdout is dropped on M:N | Latent today (the drain's cancel flag makes siblings `Cancelled`, which flushes); becomes live the moment two tasks can fault in one drain. Same milestone |
| **W7-11** | `:3901` | A `RwShared` holding a container with an element that back-references the CONTAINER (`a.next = xs; RwShared(xs).at(0)`) aborts the host on `from_wire_memo`'s `.expect("a wire Backref always targets an already-reconstructed node id")` — a legal program, no concurrency, both engines | **Pre-existing on main**, not a W7-4 regression (verified on 5960052): a copy-out view drains ONE depth-1 piece, and a piece whose cycle closes through the ROOT container can never be self-contained — the definition it needs IS the container. `elem_split` fixes the sibling-CELL case, not this ancestor case. Closing it means either a catchable fault instead of the `.expect` (`from_wire_memo` returns a `Value`, so that is a signature change through every rebuild arm) or a same-guard whole-container fallback at all 12 view sites |
| **W7-4a** | `:3901` | Airlock cell identity is preserved **per module** in the snapshot, so two globals in DIFFERENT modules over one shared cell still arrive as two cells | Residual disclosed by the W7-4 fix. Closing it needs `Vm`-lived rebuild state kept (and rooted) across the lazy per-module faults; the reported repro is same-module and is fixed |
| **W7-4b** | `:3901` | A cell whose inner value carries a residual `Module`/`Native`/`Cffi` handle falls to `SnapValue::Cell`, which has no `Backref` encoding, so its identity is not preserved across a module snapshot | Residual disclosed by the W7-4 fix, and the same limit the `SnapValue::Closure` slow arm already documents. Closing it is a snapshot FORMAT change (id/`Backref` arms on `SnapValue`), out of proportion to a residual this narrow |
| **W7-4c** | `:3901` | ONE TASK reached through TWO serializations still gets two bindings: a `spawn:` block's captures and the module-global snapshot cross into the same task but are separate memos, rebuilt at different times (the snapshot faults in lazily) | Residual disclosed by the W7-4 fix; same family as W7-4a. Closing it needs `Vm`-lived rebuild state across GC-visible points. Fenced by `module_global_plus_local_capture_still_split` and stated in `syntax.md` rule 2 |
| **W7-4d** | `:3901` | An `RwShared` COPY-OUT VIEW (`at`/`for_each`/`fold`/`get_key`/`has`/`for_each_entry`/`fold_entries`) rebuilds one piece per step, so two sibling closures pulled out separately do not share their binding | Inherent to a copy-out API — two `at()` calls are two crossings. A whole `get()`/`read()`, and `slice`, are one crossing and DO share. Not a residual of the fix, documented in `concurrency.md` §airlock |

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
wave 6 carries no open DEFECTS; what remains are three disclosed residuals W6-9r/W6-10s/W6-10r, filed as
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
(`src/native/process.rs`, declared in `std/process.chz`, both `is_blocking`) return the child's stdout
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
>   scalars allocates no `Obj`s AND charges no wire bytes, so neither trigger fires. Still open; this
>   fix does not touch it.
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
> **RESIDUAL, STILL OPEN (`W6-10r` in the index table).** The byte walk stops at a **nested-core
> boundary**: those bytes are owned by that core's own summary, and `live_bytes` reaches a core's
> summary only through an `Obj::*` alias slot. A nested core whose last alias slot has been swept —
> e.g. `s := Shared(ch)`, then the local `ch` binding dies, then backlog through `s.get().send(...)`
> — survives inside the parent's `WireValue` with no slot of its own, so its backlog is counted
> **nowhere** and sails past the cap exactly as before. Closing it needs cross-core byte recursion
> with `Arc` de-dup; deliberately not built (narrow trigger, real machinery). The earlier claim that
> "that core's own summary owns those bytes" makes the case safe was **wrong** and is retracted.
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
it is a DIFFERENT hole and remains OPEN.)
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
> 1. ~31 hand-rolled `run_file_p` + `run_file` cross-engine compares in `parity_tests.rs` (e.g. `:782`,
>    `:891`, `:4144`) still diff decoded `String`s. Converting them means rewriting call sites, which
>    the fix's shape constraint (helper-level only) forbids. A future byte-emitting test added at one of
>    those sites would inherit the blindness — use `run_file_bytes` there.
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

**Residual ceilings, shipped as documented known limits** (`ponytail:` comments at the sites). All the
same shape — TWO INDEPENDENT SERIALIZATIONS reach one cell; identity is per serialization:
- **W7-4a** — cell identity is **per module** in the snapshot: two globals in DIFFERENT modules over one
  shared cell still split. Closing it needs `Vm`-lived rebuild state kept across the lazy per-module
  faults (and rooted); the repro is same-module.
- **W7-4b** — a cell whose own inner value carries a residual `Module`/`Native`/`Cffi` handle falls to
  the `SnapValue::Cell` slow arm, which has no id/`Backref` encoding, so identity there stays wire-only
  (the same limit the `SnapValue::Closure` slow arm already documents). Closing it is a snapshot FORMAT
  change, out of proportion to a residual this narrow.
- **W7-4c** — ONE TASK reached through TWO serializations still gets two bindings: a `spawn:` block's
  captures and the module-global snapshot cross into the same task at the same instant, but are
  separate memos rebuilt at DIFFERENT times (the snapshot faults in lazily on the task's first module
  access), so their rebuild maps cannot be unified without `Vm`-lived state across GC-visible points.
  `c := make()` at module level with `gi := c.inc` a global and `gg := c.get` a local captured by the
  block reads `0`, not `2`. Same family as W7-4a; fenced by
  `module_global_plus_local_capture_still_split` and stated in `docs/syntax.md` rule 2 +
  `docs/concurrency.md` §airlock.
- **W7-4d** — an `RwShared` COPY-OUT VIEW is per-piece independent: `at`/`for_each`/`fold`/`get_key`/
  `has`/`for_each_entry`/`fold_entries` rebuild one piece per step, so two sibling closures pulled out
  separately do not share their binding (two `at()` calls are two crossings — they never could). A
  whole-container `get()`/`read()`, and `slice` (one call returning a container), ARE one crossing and
  do share. Inherent to a copy-out API, not a residual of the fix.

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
  convert once at the end.
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
