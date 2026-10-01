# Root causes behind bug-hunt wave 17 (JIT sweep #2, 2026-09-30)

Sweep #2 ran five hunters (~1 400 programs) against `main` at `77c346f4`, after the five wave-16
families were fixed structurally (`docs/root-causes-w16.md`, TICKET-177..183). Every finding below
was re-run by the judge on the release binary. The sweep is **not clean**: at least 9 new P0 and
15 P1 in the core, so it does not count toward the JIT entry rule.

This document follows `docs/lessons.md` §1 "How to find the root cause": findings grouped by the
fact they share, every decider found by grep, the grid drawn, past patches counted, and the single
source named. Status lines are added per family as tickets land.

## What sweep #2 says about the wave-16 fixes

| wave-16 family | ticket | held in sweep #2? |
|---|---|---|
| Identity | 177 | **yes** — every identity cell clean |
| Blocking contexts | 181 | **yes for its own grid** (25 ops × 26 contexts clean); one new cell (H3) and one adjacent family (B below) |
| Airlock | 179 | **partly** — unified for `spawn` only; 6 other crossing routes still decide on their own (C) |
| Declarations / module scope | 178, 183 | **partly** — fixed *when* globals are typed, not *where* their facts live (D) |
| Names / call binding | 180, 182 | **partly** — unified for expression positions and user-type calls; type positions, builtin/protocol receivers, generic values and `.decode` still resolve on their own (F) |

**Why the partial ones missed cells:** each ticket's grid was drawn from the bugs it was fixing,
not from the fact. 179's grid varied only the `spawn` form (Executor/defaults/imports appear once in
the whole grid file); 180's name grid had value/call/member positions only (no annotation, bound or
alias-body position); 182's binding grid had one protocol row with one matching implementor and no
builtin receiver. The rule this adds (recorded in `docs/lessons.md`): **derive the grid's axes from
every consumer of the fact (every route, position, receiver kind), found by grep — never from the
repros.**

Two families are new: channel hand-off (A) and control-flow summary (E).

## Findings (re-verified)

| ID | Sev | Group | Repro (short) | Chezzi | Ancestor |
|---|---|---|---|---|---|
| H1 | **P0** | A | unbuffered `send` with a receiver parked (esp. `wait:`); sender then `recv`s | sender gets its own value back; worker pool loses 2/100 (8/10 at T=2) | Go: receiver gets it, 0/200 |
| H2 | P1 | A | parked sender; receiver drains then closes | false `send on a closed channel`, value lost (10/10) | Go: `sent both` |
| H3 | P1 | A | `wait:` send arm + receiver inside a callback | false `deadlock` (9/10 at T=2; regression since 181) | Go `[1]` |
| F2 | P1 | B | nursery owner blocked in a socket op; child panics | **hangs forever** | Go: panic at 50 ms |
| F3 | P1 | B | spawned-task owner blocked in sleep/recv; child panics | owner keeps running 3 s, consumes a sibling's message | CPython TaskGroup: cut at once |
| A1 | P1 | C | callee rebinds its param then pushes; `spawn f5(xs, out)` | compile error "task's copy" | Go `3 [1]` |
| A2 | P1 | C | `Task.get()` inside a spawned task | faults **after consuming the result**; parent deadlocks | CPython `fut.result()` works |
| A3 | P1 | C | `std.memoize` wrapper called in a task | faults | CPython works |
| A4 | P1 | C | `spawn work(out)` with default `acc = []` / variadic pack | false fault (`spawn: work(out)` works) | Go/CPython work |
| A5 | P1 | C | `spawn lib.f(..., [1], ...)` (qualified callee) | fresh mask ignored → false fault | — |
| A6 | P1 | C | Executor job resumes a started generator | false fault (spawn works) | — |
| A7 | P2 | C | started generator crossing, parent holds 200k objects | 39 s (O(heap) reach scan) | CPython 0.4 s |
| K4 | **P0** | D | fn above `z := None` writes `Some("ab")`; `-> int` fn returns `z ?? 0` | prints `abab` | Go: compile error |
| K5 | **P0** | D | body above `f := fn(b, a)` calls `f(a=1, b=2)` | `21` (old fn's labels) | CPython `12` |
| K11 | P1 | D | `PI := 2.0` / `fn bump(): PI = 3.0` / `PI: const float = 3.14` | const reassigned, `3.0` | JS: SyntaxError |
| K10 | P1 | D | `m.x` where `m`'s top-level `return` skipped `x := 5` | **Rust panic** in the heap (`m.x + 1`) | CPython: SyntaxError |
| K7 | **P0** | E | user `fn exit(...) -> int`, used as a statement in an `-> int` fn | fn returns `nil` | Rust: E0308 |
| K8 | **P0** | E | `while true:` whose only `break` is inside `wait:` / `parallel:` | `-> int` fn returns `nil` | Rust: E0308 |
| E3 | P2 | E | fn whose `wait:` has every arm `return` | rejected "can fall off the end" (match equivalent accepted) | Go compiles |
| K6 | **P0** | F | named args through a protocol, a `str` value conforms | args swapped → `aaa` | CPython: TypeError |
| K9 | **P0** | F | `x.decode[T](s)` on any `x` | always compiled as `json.decode`; runs a user `parse` | CPython: AttributeError |
| G1 | P1 | F | `f := cmp.max` inside `fn pick[T]` on a struct | check OK, then `no 'compare' method` | direct call rejected |
| P1-4 | P1 | F | `import Q from lib` where lib has `type Q` + `fn Q` | `Q` unusable as a type | Rust `use` works |
| P1-5 | P1 | F | `type N = named.Named` used as a bound | "unknown protocol 'N'" (docs promise it works) | — |
| G2, G3, P2s | P2 | F | kwargs via `g := m.f`; variadic value omitting its default; local `print` in `defer` | false rejects | CPython works |
| — | P1 | isolated | `["a","b"].map(ord)` | builtin rejected as a generic fn argument | CPython `[97, 98]` |
| — | P1 | isolated | `a, _ = (1, 2)` | rejected | Go `1` |
| — | P2 | isolated | 6 tasks × 2000 `Shared.update` at T=2 | bimodal 0.03–3.6 s | Go 2.5 ms |

## Family A — channel hand-off has no commit point (NEW)

**Status (2026-09-30): fixed by TICKET-185.** One transfer protocol in `ChanState`
(`src/vm/core.rs`): a blocked sender publishes an OFFER, a blocked rendezvous receiver a SLOT, and a
value moves only by a CAS on the party's `Pending` under the channel lock (`ChanState::give` /
`ChanState::pop`); `ChanState::send` is the one send decision. `recv_waiting`, `has_send_slot`,
`RecvWait`, the deposits, the retry loops and the re-run closed guards are deleted. A `wait:` has one
`Pending` for all its arms. DEC-181's `Demote` × `Send`/`Wait+send` cells are now `Demote`. Grid:
`tests/channel_handoff_grid.rs` (5 sender contexts × 3 sender kinds × 6 receivers × 3 caps × 2
orders × 3 close cells, plus H1, H3 and the own-value repro, at T=1, 2, 0 and one seed). A wake
requeues only a parked party whose `Pending` left QUEUED (`WakeKind::Settled`; `send_keys` is
deleted), and a cap-0 `give` hands its receiver to the giver's `runnext`. Perf: accepted by the owner
2026-09-30 with a residual. Unpinned `rendezvous_pingpong` is +16% (T=0) / +25% (T=4) over base,
and ~10% faster when pinned to two distinct cores. Tracked as **W17-1** (`docs/gaps.md`); tables in
`docs/benchmarks.md` §TICKET-185.

**Fact:** "this value has been delivered to exactly one receiver."

**Deciders (3 mechanisms, 5 outcome sites):** a presence counter (`has_send_slot` =
`queue.len() < recv_waiting`, `src/vm/core.rs:396-402`, TICKET-028) lets a cap-0 send enqueue a
plain `Msg` and return as soon as *some* receiver is waiting — nothing binds that `Msg` to the
counted receiver, and a parked `wait:` receiver may then take another arm; "retry" (a parked
cap>0 sender and every `wait:` send arm discard the value and re-run `send`); and a real
commit (`DEPOSIT_TAKEN`, TICKET-042a) that exists only on one path and is read **after** the
top-of-`send` closed guard (`src/vm/netio.rs:1571`, also `:2766` for `wait:` send arms). The
W7-13r(c) "result before closed" ordering was copied into the in-place loops only.

**Grid:** sender context × sender kind × receiver kind, receiver-parks-first, cap 0: **40 of 48
cells wrong**. Sender-parks-first + take-then-close: every fiber sender wrong, every job sender
right. H3: a `wait:` send arm publishes no offer, so the TICKET-181 registry's
`PartyWait::Recv::satisfiable` (quiesce.rs:152) sees nothing to take; `block_enter` also runs before
the recv arms are armed (sched.rs:1657).

**Past patches:** 16 commits (wait-send arms 90137424/77a47a63/70aa9b2b; TICKET-028, 042 ×5,
W7-13 ×4, 117, 128, 136) — none added a commit point.

**Single source:** one transfer protocol. Every blocking sender (send, `wait:` send arm, job,
main) publishes an **offer**; a receiver **commits** by taking it atomically under the channel lock;
`send` returns only on commit; a cap-0 channel never holds a plain `Msg`, so `recv_waiting` goes.
`closed` is judged once, against the offer (`WITHDRAWN` = closed). The deadlock registry asks "is
there an offer on the other side?". Unbounded channels keep the plain queue.

**Grid:** sender context {fiber, job, main, builder, demoted} × sender kind {send, `wait:` arm,
try_send} × receiver {recv, try_recv, for-in, `wait:` arm, demoted recv/`wait:`} × cap {0, 1,
unbounded} × order {sender first, receiver first} × close {none, after take, while parked} × T {1, 2,
4}. Oracle (Go): each value received exactly once, never by its own sender; `send` fails only if
nobody took it.

**Risk:** the hottest path — an allocation + CAS per cap-0 send (TICKET-126 measured +18% for a
similar change); keep TICKET-128's runnext hand-off.

## Family B — a child's fault is polled, not pushed (NEW, next to Blocking)

**Status (2026-10-01): fixed by TICKET-188.** One decider `block::halt_of`, one wake set
`Vm::wake_set` read by every wait registration and by every quiesce party (the verdict vetoes only a
recorded child fault), and the grid `tests/owner_fault_grid.rs`.

**Fact:** "must this blocked owner stop now?"

**Deciders:** cancellation is **pushed** — a flag wakes every registered wait — and checked at
32 sites. A child's fault is only **polled** (`owned_nursery_fault()`, 3 sites: the in-place loop
rung, the loop back-edge, the HOF tick). An owner does not hold its own nursery's flag
(`src/vm/exec.rs:1876`), so every wait woken by flags — every parked fiber wait, every socket wait,
every demoted wait — never sees the fault. TICKET-181's `Waiter.cancel` is the same flag set and
feeds only the deadlock verdict.

**Grid (measured):** socket read/accept/write × every owner kind → **hang**; recv/sleep/`wait:` in
any fiber-context owner → runs until its own wake (1.5–3 s), then continues. Only main-thread
in-place waits are cut (~65 ms).

**Past patches:** TICKET-062, 096 (+20434aaa), 134, 155 — each added a poll site (~9 counting the
wider cancel-point family); W11-14 already named the pattern.

**Single source:** one wake set, `wake_flags() = cancel flags ∪ this owner's own open nurseries`,
used everywhere a wait registers for cancellation (Waiter, netpoller, channel park, timer, demote
loops), and one resume predicate `halt_requested()` replacing both `cancel_requested()` (32 sites)
and the 3 owner-fault polls — it returns "cancelled" or "your child faulted (with its error)".

**Grid:** every op in `block.rs` × owner kind {main, fn in main, spawned task, fiber-owned nested
nursery, Executor-job owner} × context {plain, callback, generator, `Shared.update`, `defer`} →
cut within 200 ms, no line after the op, no sibling message consumed; `defer` cells assert NO cut.
Generate from `block.rs`'s own context/spec lists.

## Family C — airlock: "can the parent observe it?" still decided per route (Family 4, incomplete)

**Status:** C1 landed (TICKET-189); C2 landed (TICKET-190); C3 Task landed (TICKET-191); C3 memoize landed (TICKET-192). One checker crossing policy per
bound slot (`crossing_of` + `bound_slots`, `CrossingTable`), encoded by `vm::crossing::Crossing::mask`
on every spawn form, route constants only in `vm::crossing::marks`; layer A maps through the call plan
and drops a rebound param. Generator frames: one static mask per proto (`Proto.private_slots`) ANDed
with the creating call's param stamp, read by every route; `gen_frame_observable` deleted. A1, A4, A5,
A6 and A7 fixed; A2 fixed by TICKET-191; A3 fixed by TICKET-192 (`memoize1` caches in an `RwShared[Map]`).

**Deciders (7):** checker freshness (`spawn_operand_is_fresh`, sig.rs:2505) → compiler mask
(`fresh_mask`, `fresh_mask_srcs`: a default fill or pack is "never fresh", compiler/mod.rs:5095);
layer A (`report_named_call_writes` + `fn_writes.rs`, own alias test, never reads the freshness
table, never drops a rebound param — A1); spawn-block capture writes; the spawn rebuild mark
(sched.rs:5052/5110); generator frames (`gen_frame_observable`, spawn crossings only — A6; O(heap)
— A7); closure captures (`origin_heap` always marks, sched.rs:4531); module snapshot (always marks,
sched.rs:6287/6362). Qualified spawn emits `Op::SpawnCall(n, 0)` — mask hard-coded to 0 (A5).

**Std breaks D4's own rule:** `Task` keeps its one-shot result in a by-value struct field and
`memoize` keeps its cache in a captured `Map`; both are shared state that crosses tasks. Go
(channel + `sync.Once`) and Python (`Future` is one locked object) make them shared handles.

**Single source:** one `crossing_policy(operand site) -> Move | Copy(mark) | Share`, decided once
by the checker per operand (receiver, argument, default fill, variadic element, captured slot) and
consumed by every crossing site (spawn, Executor submit/map, Channel, Shared/Atomic, closure,
module snapshot, generator); layer A reads the same policy and tracks rebinding; generator-frame
observability as a static per-proto slot bitmask (O(frame)). Std: `Task` over a `Shared` core,
`memoize` cache as `Shared[Map]`.

**Missing grid axes (why 179 missed these):** crossing route beyond spawn; callee form; argument
source (explicit/keyword/default/variadic); param rebound in the callee; user vs std writer;
generator state × route; heap size (perf cell).

## Family D — module scope: no single record per global (Family 3, incomplete)

Status: fixed by TICKET-186 — one GlobalBinding record per module slot (src/checker/globals.rs) and one runtime reader Vm::read_slot; module-level return and const/plain mixes are rejected.

**Deciders:** 15+ tables keyed by name (`scopes[0]`, `const_decls`, `kw_certain`/`kw_written`/
`kw_pending`, `written_captures`, `empty_coll_sites`, `carrier_pins`, `unreached_globals`,
`seeded_globals`, `cycle_globals`, `functions` labels, `fn_reads`, …), hand-synced by `declare` and
the `Let` arm. TICKET-183 seeds three facts (type, const, kw_certain) before bodies — then the first
`let` **wiped the seed** (`sig.rs:2551-2555`, deleted by TICKET-186: the first let now refines it) and rebuilt from walk order. A body is checked against
the state at its source position but runs against the state at call time (K4, K11). The redeclare
guard compares `Ty`, and `FnLabels` equality is always true, so a rebinding with swapped parameter
names passes (K5). At runtime the uninitialized check lives on 3 opcodes; 4 read paths (qualified
`m.x`, `module.fn()`, the entrypoint, `module_global`) skip it, and the in-band `UNINIT` value
escapes as a normal value → heap panic (K10).

**Single source:** one `GlobalBinding` per module slot (type, const, param labels + arity, written,
refinement, init state), computed by a whole-module pass before any body or top-level statement;
the `Let` arm refines it and never removes it; every checker reader consults it. At runtime one
`read_slot(module, idx)` that faults on uninit, with no raw slot access outside it.

**Grid:** fact × writer position {above first let, below, between two lets} × read path {bare, closure
capture, qualified `m.x`, from-import, `module.fn()`, entrypoint, spawn snapshot}.

**Owner decisions (2026-09-30):** (1) a global declared twice with one plain and one `const`
declaration (either order) is a **compile error** (JavaScript `let`/`const` redeclaration rule);
(2) a **module-level `return` is rejected** at compile time ("'return' outside a function", CPython's
SyntaxError), which removes K10's path at its source — the runtime `read_slot` check is still added
for every read path.

## Family E — "can this code fall through?" decided by 7 walkers (NEW)

**Status (2026-09-30): fixed by TICKET-184.** One walker, `src/checker/flow.rs` (`flow::stmt` /
`flow::block`, every statement kind incl. `wait:` and `parallel:`), feeds missing-return, the
`recover:` tail, inline-body inference, the `recover:`/`defer:`/`spawn:` escape checks and fn_writes'
"left". Divergence comes from the resolved callee (`Checker::resolution_diverges`), never a name.
`stmt_terminates`, `block_terminates`, `stmt_has_break`, `block_has_break`, `expr_is_diverging_call`,
`escaping_flow` and `imported_diverging` are deleted. The compiler traps after an end the checker
proved unreachable (`NoFallOffTable`) instead of returning `nil`. Grid: `ticket184_flow_grid`.

**Deciders:** `stmt_terminates`, `stmt_has_break`, `expr_is_diverging_call` (a **name test** on
`exit`/`panic`/`.exit`, sig.rs:4734), `escaping_flow`, fn_writes' "left", the type-side divergence
facts (these ARE resolved correctly), and the compiler's unconditional `Op::Nil; Op::Return`
(compiler/mod.rs:1279), which turns every miss into a silent `nil`. Walkers 1–3 take `&Stmt` with no
checker access, so they cannot read the recorded `Resolution`; each keeps its own statement list —
`escaping_flow` knows `wait:` and `parallel:`, `stmt_has_break` and `stmt_terminates` do not.

**Single source:** one `flow(stmt) -> {falls_through, breaks, continues, returns}` summary computed
by `check_stmt` as it walks (every statement kind), with call divergence read from the resolved
callee's type. Every consumer derives from it.

**Grid:** statement kind × {break / return / diverging call at each nesting position} × consumer,
including shadowed `exit`/`panic` (local, user fn, method) and qualified `os.exit`.

## Family F — names: type positions, receivers, generic values, `.decode` (Family 1)

**Status: fixed by TICKET-187 (2026-09-30).** Four single sources replace the deciders below:
(1) a call's parameter list — `callable_slots` builds the slots of every callee with no declaration
(protocol requirement, fn value, with its variadic slot), `param_name_mismatch` is the one
name-conformance test, `names_builtin_fn` is the one "this head is the builtin" test; `lend_specs`,
`lend_bind` and `desugar::collect_methods` are deleted. (2) Type positions — both alias hand walks
read `resolve_ty_ro`, and `bind_imported_type` binds a from-import's type whatever its value twin is.
(3) Generic fn values — `generic_fn_value_sig` / `generic_fn_value_ty` pin or reject every read
(bare, from-import, qualified). (4) `.decode[T]` — `ExprKind::DecodeCall` and the parser steal are
deleted; only `decode` on `std.json` records `Resolution::Decode`. Grids:
`tests/call_binding_grid.rs` and `tests/name_resolution_grid.rs` (`t187_cells`).

**Facts and deciders:**
- *A call's parameter list:* 6 deciders. `bind_call` covers user types only; builtin receivers have
  no parameter names; protocol receivers bind against `lend_specs`, a table keyed by **method name**
  across every user struct (not the protocol's own names, no builtin conformers — K6); fn values
  rebuild slots from `FnLabels` with `is_variadic: false` (G3); keyword calls through a value are
  gated by syntax (`let_holds_one_known_fn` accepts only a bare `Ident`, G2); `print` special-cased by
  name string in `defer`/`spawn`.
- *What a type-position name means:* 4+ resolvers (annotations, alias bodies, bounds —
  `protocol_alias_key` follows only bare `Named` bodies, P1-5 — and the from-import if/else chain
  that binds a type twin only for struct/newtype, P1-4). TICKET-180 moved expressions and patterns to
  one table; type positions still resolve by string.
- *A type parameter's identity:* `Ty::Param(String)` compares by name, so a caller's `T` merges with
  an imported generic's `T`; the module-member value read has no generic handling (G1).
- *`.decode[T]`:* decided by the **parser** (every `x.decode[T](s)` becomes json decode), the
  checker discards the receiver's type, the compiler hardcodes `parse` (K9).

**Single sources:** one `callable_slots(resolution)` (slots incl. variadic on `FnSig`/`Ty::Func`;
native methods named from their `.chz` declarations; protocol calls bound by the protocol's declared
names; delete `lend_specs`); one type-position resolver recording a resolution per type node;
imports bind the value and type namespaces independently; `Ty::Param` carries its owner (or fresh
instantiation at every value read, through one `fn_value_ty(resolution)`); `.name[T](args)` parsed as
an ordinary member call, with only `std.json`'s decode resolving to `Resolution::Decode`.

**Missing grid axes (why 180/182 missed these):** receiver kind (builtin, protocol with a builtin
conformer, permuted-name implementor, several implementors); fn-value source (bare, qualified,
from-import, generic, variadic-with-default); position (annotation, bound, alias body, import with a
fn/type twin); same-named vs differently-named type params; syntax-special forms under shadowing.

**Owner decision (2026-09-30):** named arguments on a protocol method call bind to the
**protocol's declared parameter names**, and an implementor's method conforms only if it uses the
same parameter names (the interface is the contract). Builtin conformers bind by the same names.

**Owner decisions (2026-09-30, from TICKET-187 planning):** (1) a protocol's parameter list is the
whole contract: a call through a protocol that omits an argument is an arity error even when every
implementor declares a default (Go and Rust have no defaults through an interface). This deletes the
name-keyed default lending (`lend_specs`) and reverses DEC-075's W7-51 lending. (2) The name rule
applies to every protocol method of every arity, user and prelude protocols alike (names are part of
the signature, as Swift's argument labels are); `eq(self, o)` must be `eq(self, other)`. (3) A
qualified generic read with no determinable type argument reports `is generic and T is not determined
here` (Go: `cannot use generic function cmp.Max without instantiation`).

## Isolated (fix in place, no family)

- a builtin fn (`ord`) passed where a generic fn type is expected is rejected; **fixed 2026-10-01**: generic inference reads both fn variants through `Ty::fn_parts` (`tests/chz/spec/builtin_fn_generic_arg_test.chz`);
- `a, _ = (1, 2)` rejected; **fixed 2026-10-01** (`Expr::is_blank`, `f3822211`);
- contended `Shared.update` at T=2 is bimodal (0.03–3.6 s) — measure before deciding it is a defect; **measured 2026-10-01: a defect**, 15.6–19.6 s at T=2 on every run (T=1 0.03 s, Go 0.007 s), present before wave 17 too; **TICKET-193**;
- `docs/syntax.md:2057` names `Comparable` where the error says `Eq`. **Fixed 2026-10-01** (the example now quotes the real error).

## Plan

| order | family | size | why this order |
|---|---|---|---|
| 1 | E control-flow summary | small–medium | P0 silent `nil`; checker-only; unblocks nothing but is cheap |
| 2 | A channel hand-off | large | P0 data loss in the Go worker-pool idiom; hottest path — own milestone, measured |
| 3 | D module-scope binding record | medium | P0 type holes + a heap panic; needs the two owner decisions |
| 4 | F names residue | large | P0s; needs the protocol-named-args decision; split into slots / type positions / param identity / decode if too big |
| 5 | B wake set for owner faults | medium | P1 hang; builds on 181's `block.rs` |
| 6 | C airlock crossing policy | medium–large | P1 false faults (no lost writes); std `Task`/`memoize` redesign |

Each ticket must derive its grid from every consumer of the fact (routes, positions, receiver
kinds) by grep, per the rule above, and land with it.
