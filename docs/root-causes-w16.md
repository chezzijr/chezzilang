# Root causes behind bug-hunt wave 16 (2026-09-28)

Wave 16 was JIT-entry sweep #1 (`PROGRESS.md` "JIT entry rule", condition 2). Five hunters ran about
550 hand-built programs against the release binary at `8b1d2331`. They found **5 new P0 and 7 new P1**
in the core, all re-verified by the judge. So the sweep does not count toward the JIT rule.

The owner's question was not "how do we fix these twelve" but **"why does every sweep find more?"**.
This document answers that. Each finding was traced to its mechanism in the code, and each family was
checked for how many earlier tickets patched the same class.

## The finding in one sentence

**One fact is decided in several places, kept in sync by hand, and every bug is a place where the
copies drifted.** Each earlier fix patched the one copy that drifted, so the family kept producing
bugs.

Two wrong assumptions sit under the families:

1. **"The checker and compiler walk the same AST, so they reach the same answer."** They do not.
   They have different scopes, different tables and different lookup orders. The compiler is
   deliberately type-blind, so it rebuilds meaning the checker already worked out.
2. **Assumptions left over from an earlier design.** "One heap" (identity by heap slot), "declarations
   only at the top level", and "a value that crossed the airlock is the parent's" were each true or
   close enough at some point. The design moved on (per-task heaps, a block grammar, fresh values in
   spawn arguments) and nothing re-checked them.

Why sweeps keep finding more: each family is a **grid**, for example 12 blocking operations × 7
execution contexts, or 10 kinds of name × 8 kinds of binding. A hunt samples random cells, and a point
fix repairs one cell. Only moving the decision to one place removes the grid.

## Sweep findings (all re-verified on the release binary)

| ID | Sev | Family | Repro (shortened) | Chezzi | Ancestor |
|---|---|---|---|---|---|
| K1 | P0 | Names | `ord := fn(s: str) -> int: 1000` then `ord("a")` | `97`, the builtin ran | Python/Go `1000` |
| K2 | P0 | Names | `type Q = P`; inside `fn Q(s: str) -> P`, `return Q(s[1:])` | `P(x='bc')`, a `str` in an `int` field | Rust/Python `P(x=0)` |
| K3 | P0 | Names | `lib3` has `type Q = P` and `fn Q(s: str)`; `lib3.Q(5)` | checker types the ctor, VM runs the fn | Rust: compile error |
| S2 | P0 | Declarations | `struct S` inside `main()` shadowing a top-level `S` | outer `S` used, silently | Python `inner` |
| C2 | P0 | Identity | `c2 := c1`; inside `spawn:` `c1 == c2` | `false` | Go `true` |
| C1 | P1 | Blocking | `wait:` with a timer arm inside `[1,2].map(pick)` in a spawned task | false fatal `deadlock` | Go `[10 20]` |
| X1 | P1 | Blocking | `Executor.shutdown()` in a spawned task at `CHEZZI_THREADS=1` | hang, 3/3 | Go `GOMAXPROCS=1` completes |
| X2 | P1 | Blocking | `peer.read(2)` inside `.map()` or `defer` on main | immediate `Err` naming a nonexistent Executor job | Python/Go block and read |
| A1 | P1 | Airlock | `spawn work([], out)`; `work` pushes onto its argument | false "task's copy" fault | Go/Python `2` |
| A6 | P1 | Airlock | `m.merge({...})` inside a task (`merge` returns a new map) | false fault; pinned by a test | not a write |
| A2 | P1 | Airlock | generator started in the parent, drained in a task | fault on its own frame-local list | Python/Go work |
| S1 | P1 | (arithmetic) | `xs[1::9223372036854775807]` | Rust panic, even under `recover:` | CPython `[1]` |

P2 rows (diagnostics, flow-insensitive layer-A reject, `bytearray` `.copy()` hint, `parse_int_base("017", 0)`,
`rsplit` with an overlapping separator, several false rejects in alias/keyword-argument corners) are
listed in the wave-16 ledger rows.

## Family 1 — Names (K1, K2, K3)

**Mechanism.** The checker resolves each call head (`src/checker/expr.rs`) with the order: local
binding → type parameter → builtin → newtype ctor → struct ctor → user fn. It uses the answer to
compute types and then discards it. The compiler (`src/compiler/mod.rs` ~5470–5630) re-resolves from
the bare string with the order: hardcoded runtime ctors → prelude builtins → newtype → struct → variant
→ user fn, and consults `fc.is_unbound(name)` **only after** every ctor and builtin arm. Locals therefore
lose to builtins and types (K1). `raw_ctor_owner` is set by the checker only for struct/newtype names
(`src/checker/sig.rs:457-460`) but by the compiler for anything in `bare_types`, which includes aliases
(`src/compiler/mod.rs:1203`, `:1361-1366`) — K2. The checker's qualified alias-head path
(`src/checker/setup.rs:266`, TICKET-172) skips the `sig.functions` precedence that the compiler's
`module_fns` applies — K3.

**Duplicated decision.** About 20 compiler sites re-resolve a name by their own rules: bare calls,
ctors, variants, statics, turbofish heads, patterns, `compile_ident`, JSON decode type names, `defer`
and `spawn` call splits. The 8 existing checker→compiler side tables (`ExternTable`, `KeywordTable`,
`WitnessTable`, `CarrierTable`, `ProtoEqTable`, `SumSeedTable`, `RetCoerceTable`, `ForBindTable`)
carry types and permissions, **never which declaration a name resolved to**.

**Past patches of this class:** at least 14 — W6-6, W6-11, the FFI collision fixes, 853a4dd1,
44dafc41, TICKET-029, TICKET-055, M24-2, M24-3, W7-53 I1′, dafc2a4b, a24d8cf7, a10c1043, TICKET-172.
`docs/lessons.md` already states the rule ("put one shared predicate … so they agree by construction")
but it was only ever applied to type predicates, never to name resolution. `ExternTable` is the one
precedent where a second resolver was actually deleted.

**Structural fix.** The checker records a `Resolution` for every call head, identifier read and pattern
head (`Local`, `Capture`, `Global`, `Fn`, `Builtin`, `StructCtor`, `NewTypeCtor`, `Variant`, `Static`,
`ModuleMember`). The compiler consumes it as its **only** source and treats a missing entry as an
internal error (as `keyword_perm` already does). Delete the ~20 re-resolution sites, `ctor_shadowed`
and the compiler's alias entries in `bare_types`.

**Keying.** `Expr` is `{kind, span}` (`src/ast/mod.rs:886`) with no node id, and Span-keyed tables
alias (`docs/lessons.md`, four recorded instances). The table needs a node id assigned by the parser
and renumbered by desugar for every clone or synthesized node, keyed `(module_idx, NodeId)`.

**Risks.** Wide AST change; desugar clones must get fresh ids or the aliasing returns; `Expr` grows
(affects the `MAX_DEPTH` sizing); `resolve_call_tables` must fill the table in the typing pass.
An interim shared `resolve_callee` fixes order drift but not scope-model drift and is a stepping stone
only.

**Size:** large — its own milestone.

## Family 2 — Blocking contexts (C1, X1, X2)

**Mechanism.** No single "blocking context" exists. Each blocking operation derives "may I block here,
and how?" from `mn`, `native_reentry`, `eager_core`, `deferring` and `holds_width` in its own order —
at least 11 distinct predicates (`can_block_in_place`, `owns_os_thread`, `is_counted_party`,
`may_block_socket_in_place`, the demote conditions, the full-send fault, …). Each demote path also
picks its own accounting bucket: a demoted recv or `wait:` is `blocked_native` ("only a sibling can
wake me"); a demoted sleep, stdin read or socket op is `inflight` ("I will return"). The deadlock
detector does not inspect blocked waiters; it keeps 15 hand-maintained veto entries
(`src/vm/mod.rs` `is_deadlocked` / `quiesced_core_given`) that must be extended for every new demote
shape.

- **C1:** a `wait:` with a timer deadline is really "I will return" but is filed as `blocked_native`;
  only the waiter's own verdict checks `timer.is_none()` (`src/vm/sched.rs:1932`). Every other judge
  sees nothing live and faults.
- **X1:** `Executor.shutdown` in a fiber uses a condvar poll; `yield_pool_slot` returns immediately
  when `mn` is set (`src/vm/sched.rs:2292-2297`) and there is no `demote_enter`, so the worker is held.
- **X2:** `may_block_socket_in_place` requires `native_reentry == 0` on main, contradicting
  `is_counted_party` and the channel path, and the fallback error text names an Executor job.

The investigation found four more disagreeing cells: a timed `wait:` inside a `defer:` in a fiber,
inside `Shared.update` in a fiber, `wait: ch.recv() / timer` in a fiber callback with no sender, and
a closed arm plus a timer in a fiber callback.

**Past patches of this class:** about 35 (archive rows W7-14, W7-18, W7-40, W7-47, W7-56..60, W8-8,
W10-1, W10-15, W11-3, W13-8, the Executor reentrant-shutdown fix, and tickets 040, 052, 062, 063, 099,
101, 103, 112, 117, 118, 125, 127, 129, 131–134, 136, 141, 151, 155, 176). Each added a predicate
clause or a veto entry.

**Structural fix.** One `BlockCtx` computed per fiber state change (`Park`, `Demote`,
`OwnThread { judged }`, `PoolJob { in_cb }`); one `block_on(WaitSpec, ctx)` that every blocking
operation calls; accounting derived from the operation (a deadline or external completion means "will
return"); the deadlock detector derives its vetoes by scanning one registry of blocked waiters and
asking each whether it can be satisfied. `Executor.shutdown`, stdin and sockets become ordinary
`block_on` callers.

**Risks.** The hottest, most lock-ordered code in the runtime (P→A→Q order); a registry insert on the
channel fast path costs (TICKET-126 measured +18% for a similar scan); the judged/unjudged party line
must be preserved exactly or safe hangs become false faults; some differences are deliberate
(socket-in-Executor-job `Err`, full-send-in-callback v1 limit) and must become explicit table entries.

**Size:** large — its own milestone.

## Family 3 — Declarations in blocks (S2)

**Mechanism.** One statement parser (`Parser::parse_stmt`, `src/parser/mod.rs:493-603`) serves both
module level and every block, and accepts every declaration keyword in every position. Position is
policed by a three-keyword denylist keyed on the recursion counter (`self.depth > 1` for `extern`,
`native`, `import`, `src/parser/mod.rs:505-520`). The checker (`check_module` → `collect_names`,
`hoist`) and the compiler (`hoist_types`, method compilation, fn and test registration) harvest
declarations only from the module's top-level statements. In block position the compiler turns
`Struct | Enum | NewType | Protocol | TypeAlias` into `Ok(())` (`src/compiler/mod.rs:1898-1913`), and
the checker validates a nested struct's methods against the **outer** struct of the same name.

| kind in a block | result today |
|---|---|
| `fn` | correct: a real local function (documented) |
| `test fn` | silently not a test; runs as a plain nested fn |
| `struct` / `enum` / `newtype` / `protocol` / `type` | ignored; the outer namesake is used, or `unknown name` |
| `import` / `extern` / `native` | parse error (the denylist) |

**Past patches:** 3 — each of `import`, `extern`, `native` was banned one at a time.

**Decision (owner, 2026-09-28): type declarations are top-level only.** Only `fn` is a legal
declaration inside a block.

**Structural fix.** Split the grammar and parser into `<item>` (module level: `fn`, `test fn`,
`struct`, `enum`, `newtype`, `protocol`, `type`, `import`, `extern`, `native …`) and `<blockStmt>`
(simple statements, control flow and nested `fn` only). `parse_module` calls `parse_item`; `parse_stmt`
no longer dispatches item tokens and reports "`X` must be a top-level declaration". Delete the
`depth > 1` denylist and the one-off comments in `docs/grammar.bnf`. Any future top-level-only keyword
is then rejected in blocks by construction. Open sub-decisions: nested `test fn` (recommended: parse
error) and top-level `return` (parses today and ends module execution; undocumented).

**Size:** small to medium.

## Family 4 — Airlock task copies (A1, A2, A6, A3)

**Mechanism.**

- *What is a task copy.* One flag, `Vm::copy_mark` (`src/vm/mod.rs:1051-1056`), marks every object
  allocated while an airlock rebuild walk runs (spawn captures, spawn arguments, spawn receivers, the
  global replay, crossing-closure captures; `src/vm/sched.rs:5125-5128`, `6324-6332`, `6400-6408`,
  `4611-4626`). The predicate is "arrived by snapshot". D4's actual claim is "a write the parent could
  observe after the join". A fresh `[]` argument (A1) and a generator's own frame-local list (A2) arrive
  by snapshot but are not observable by the parent. Channel `recv` never sets the mark, so the same
  values sent over a Channel are never checked.
- *What is a write.* Two hand-maintained lists: the runtime's `is_mutating_native_kind`
  (`src/vm/call.rs:18-37`, 17 entries, **includes `Map.merge`**) and the checker's `mutates_receiver`
  (`src/checker/mod.rs:3865-3884`, correctly excludes it). The drift test exempts `merge` instead of
  failing (`src/checker/tests.rs:25686`) and a spec test pins the wrong fault
  (`tests/chz/spec/airlock_task_local_fault_test.chz:208`). A6. Separately, advancing a copied
  generator (`generator_next`, `src/vm/exec.rs:548`) is never checked — a real lost write that is
  silent.
- *Layer A.* `fn_writes.rs:183-213` unions the writes of every `if` branch and loop body and reports them
  as proven, while D4 says layer A "must never reject a program that would not have faulted at runtime"
  (`docs/decision-d4-airlock.md`). A3.

**Structural fix.**

1. One source for "mutates its receiver": a marker on the method declarations in `std/prelude.chz`,
   read by both the checker and the VM. Delete both hand lists, the exemption and the wrong pin.
2. Fresh values move: a spawn argument or receiver whose expression no parent binding can reach
   (literal, constructor, call result, `.copy()`) is rebuilt unmarked. Cannot create a silent lost
   write.
3. Conditional writes decline in layer A (left to layer C), as D4 requires.
4. Either check `generator_next` on a copied generator (fault honestly on the resume) or add
   liveness ("the parent never reads this binding after the spawn → move") — also fixes A2 when the
   parent drops the generator.

The deeper source is the snapshot model itself (Go/Python share memory; Chezzi copies), which D4
deferred until after the JIT. Every mark rule is an approximation of "the parent can observe it".

**Size:** medium.

## Family 5 — Handle identity (C2)

**Mechanism.** `values_equal_guarded` (`src/vm/arith.rs:2312`) is the single equality function behind
`==`, `in`, `contains`, `index_of`, `unique`, map/set probes and `Atomic.cas` (49 call sites). Its only
identity test is the heap-slot shortcut `ha == hb`. There is no arm for `Channel`, `Shared`,
`RwShared`, `Atomic`, `AtomicInt`, `Executor`, `Socket`, `Listener`, `Writer` or `Reader`; they fall to
`_ => Ok(false)` (`:2560`). Every crossing allocates a fresh slot around the same `Arc` core
(`to_wire` / `from_wire_memo`, `src/vm/sched.rs`), even two aliases captured by one spawn. `Ptr` was
already fixed the right way (compare the address, `arith.rs:2548-2551`, with a comment explaining the
multi-heap reason); the ten `Arc`-core handles never were. `std.cancel.Token` (a struct of `Shared` and
`Channel`) is therefore unequal to itself after crossing.

**Structural fix.** One helper `Obj::core_id(&self) -> Option<*const ()>` and one arm before
`_ => Ok(false)` comparing cores with `Arc::ptr_eq`. All 49 call sites inherit it. A new handle kind
added to the helper cannot be forgotten, and the same helper would serve hashing if handles ever
become hashable.

**Size:** small.

## Not a model error

- **S1** (slice step overflow): `slice_indices` (`src/slice.rs`) does `i += step` unchecked. One
  guard in the shared function covers list, str, bytes, bytearray and range. The general rule — use
  checked arithmetic on user-controlled index math — applies to the other index helpers too.
- `parse_int_base`, `rsplit`: stdlib behaviour differences; fix against CPython.

## Plan

Order by size and payoff:

1. **Identity** — small; closes one P0.
2. **Declarations** — small to medium; closes one P0 and the silent `test fn`.
3. **Airlock** — medium; closes four P1 and the silent generator write.
4. **Names** — large, own milestone; closes three P0 and removes the historically most productive
   class.
5. **Blocking** — large, own milestone; closes three P1 plus four newly found cells.

Every family's fix lands with **one exhaustive grid test** that enumerates the whole matrix (every
binder × every name kind; every blocking operation × every context; every declaration kind × every
position; every native method × "mutates?" against its declaration; every handle kind × every
identity-sensitive operation). A new operation, context or handle kind then cannot ship without a row.
`vm::tests::intrinsic_grants_all_have_vm_arms` is the existing precedent.

The point-fix items (S1, `parse_int_base`, `rsplit`) are done in place.

## Audit of earlier fixes (TICKET-120..176)

**Question (owner): did the earlier waves fix root causes?** Mostly no. Every ticket from 120 to 176
was judged from its diff, not its commit message. 127 and 133 were rejected with no commits; 174 was
folded into 172; 160, 162 and 163 were features.

| verdict | count | tickets |
|---|---|---|
| STRUCTURAL — removed a duplicated decision or made the class impossible | 9 | 122, 131, 137, 138, 153, 154, 157, 161 (one fact), 167 |
| MIXED | 8 | 135, 136, 140, 142, 151, 165, 169, 175 |
| POINT-PATCH — one more clause, veto, guard or mirror at one site | 34 | the rest |

Inside the five systemic families only:

| family | tickets | structural | point / mixed | re-hit in wave 16 |
|---|---|---|---|---|
| Blocking | 17 (125, 129, 131, 132, 134, 135, 136, 141, 147, 151, 152, 155, 159, 166, 168, 176) | 1 (131, a deletion) | 16 | C1, X1, X2 + 4 new cells |
| Names | 8 (120, 139, 142, 161, 172, 173, 175, part of 150) | 0 full (161 per fact) | 8 | K1, K2, K3 |
| Airlock | 6 (137, 154, 165, 169, 170, 171) | 2 (137, 154 — both deletions) | 4 | A1, A2, A3, A6 |
| Identity | 1 (144) | 0 | 1 | C2 |
| Declarations | part of 142, 150 | 0 | 2 | S2 |

Concrete examples from the diffs:

- **Blocking, 125 → 176:** the deadlock verdict gained a provable-leaf scan (e6398f93),
  `may_fault_unproven` + `victims_proven` (78b8e07f), a re-read after the verdict (24985fd9); per-op
  predicates gained `park_escaped_abort`'s own `native_reentry` guard (99ea4779), four
  `demote_*_block_in_place` functions (921c16a3), a `native_reentry == deferring` clause (2d129e94),
  `cancel_at_join` (d1e7289c), `claim_blocked_body_helpers` (51f722a6), `body_gate` (88f8338a) and
  `done_latch` / `demoted_groups` veto clauses (5244bfb1). TICKET-166 → 176 is a direct re-hit: 166
  made `close()` wake a parked op, the write path still returned `Ok`, and the next seeded run found it.
- **Names:** 120 added a desugar resolver that "Mirrors `find_proto_qualified`" (ebadb2e0); 139 made
  the compiler's capture analysis guess "binding or nullary variant" (60179937); 142 spread
  `if name != "_"` over ~10 sites in both phases (04c4ae03); 172 added a third alias walker, the
  compiler's `alias_member_key`, commented "Mirrors the checker's `alias_body_ty`" (1dc780de), and a
  desugar alias chase (f559d344); 173 added a "twin" key function (ee34475d). The compiler carries four
  "Mirrors the checker" comments today.
- **Airlock:** 170's first commit exported the VM's `is_mutating_native_kind` "for the checker"
  (ecdc2f21); the next commit hand-synced the checker's own `mutates_receiver` instead (05e42701).
  The checker still never calls the VM list (only a test does), which is how A6 survived. 169 narrowed
  the copy mark with a heap-id test (fe760fa3), so "copy" still means "arrived from another heap",
  which is A1/A2. 171 needed three follow-up invalidation commits (a341743b, e03194c0, 025db00f)
  because layer A is flow-insensitive — A3.
- **Identity:** 144 fixed `Atomic.cas` on a fn payload in three commits across two layers
  (e59a474e, 1122ac9c, 87f5233d) instead of defining identity once; C2 is the same fact for handles.

**What the structural wins have in common:** they deleted code. D1 (135's `is_deadlock` funnel),
D2 (137, −96 lines of airlock install), D3 (138, −1 824 lines incl. `Op::CoerceFloat`, which undid
124's per-sink patches three days later), 154 (deleted the duplicate split encoder, −240 lines) and
131 (deleted a worker-count special case). Each came from an owner decision or a ticket that asked
"which copy do we delete?". Without that ruling, a ticket defaulted to "add a clause at the site in the
repro".

**The written rules pushed the wrong way in places.** `docs/lessons.md:92-94` ("one shared predicate …
agree by construction") was applied once (142's constant evaluator) and ignored by 120, 124, 139 and
142's `_` handling. Elsewhere the lessons endorse mirroring (":146 a `native struct` mirror must seed
both harvest paths") or adding vetoes (the deadlock lesson "restrict the verdict and decline everything
else"), which is exactly what 125, 129 and 134 did. There is no rule for blocking or airlock registries.

**Process change this implies.** A ticket in a systemic family must answer "which duplicate does this
delete, or which single source does it add?" before implementation; "add a clause next to the failing
site" is a rejected plan unless the owner accepts it as a documented interim. The lessons should gain
a "derive, don't mirror" rule and drop the advice that endorses keeping two copies in sync.
