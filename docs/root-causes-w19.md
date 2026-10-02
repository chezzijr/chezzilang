# Root causes — bug-hunt wave 19 (JIT sweep #4, 2026-10-02)

Sweep #4 ran five hunters (airlock/generators, cancel/defer/net, channels/scheduler, checker/names,
stdlib vs CPython) on `main` at `e5da4fd6`, with extra weight on the code TICKET-194..198 merged.
Every finding below was re-run by the main loop on the release binary.
**Not clean: 3 P0 and 5 P1.** The JIT entry rule's "two consecutive clean sweeps" count stays at zero.

The pattern this wave: two findings are old (H1 predates wave 17), two are regressions of a fix
(H2 from TICKET-185's hand-off, C1 from TICKET-118's ancestor scan), and the checker findings are
the bare-name premise wave 16 named, surviving in three consumers TICKET-186/196/197 did not convert.

## Findings

| ID | Sev | Repro (short) | Chezzi | Ancestor |
|---|---|---|---|---|
| H1 | P0 | nursery in a spawned task (`recover:` optional), a child panics while a sibling ping-pongs, x50 | Rust panic `index out of bounds: the len is 1 but the index is 1` at `src/vm/mod.rs:5328` (`scope_family` in `cancel_drain`); 5/10 runs at T=4, 4/10 abort rc=101, else a worker dies silently | Go `done`, 10/10 |
| K6 | P0 | top-level `fn f(a, b)`, `fn o(b, a)`, `f := o`, `g := f`, `g(a=1, b=2)` | `o a=2 b=1` (f's labels; defaults stale too: `o a=1 b=9 c=2`) | CPython `o a=1 b=2` |
| K2 | P0 | `fn need[S, B: Conv[S]]`; `fn go[X, Y: Conv[str]](y: Y, x: X) -> X: return need(y, x)` | check OK, runtime `cannot apply Add to str and int`; a variant prints `i3` as an `I` | Go/Rust reject |
| K3 | P0/P1 | `struct Box[T]: fn pick[U: Conv[T]](self, u: U) -> T` | caller with its own `T`: accepted, wrong; otherwise false reject `unknown type 'T'` | Rust compiles / rejects the wrong one |
| H2 | P1 | T=1: rendezvous ping-pong between an outer-nursery sender and an inner-nursery receiver | hang 10/10 (inner sibling never runs); T=2 fine. Regression from 3164b0e5 (TICKET-185) | Go GOMAXPROCS=1 `done` |
| C1 | P1 | job's nursery fiber parked in `ln.accept()`/`c.read()`; `ex.shutdown_now()` | hangs forever, no defer runs (recv/sleep/guard parks are cut in ~120 ms) | CPython TaskGroup cancel: defers, then returns |
| K5 | P1 | `import exit from std.os; exit := fn(c: int): ...`; fn ends with `exit(3)` | check OK, runtime `internal: function 'f' fell off the end` | Rust E0308 |
| K1 | P1 | `k := 1; fn f(k: int = k)`; `g: fn(int)->int = fn(x: int) -> int: x*2` beside param `x`; `[x for x in ..]` | `default value cannot reference parameter 'k'` | CPython accepts (`1`, `6`) |
| A1 | P1 (owner) | owner `xs := t.get(); xs.push(99)`; a spawned task reads `xs` and `t.get()` | `captured xs: [1, 99]  t.get(): [1]` (same for `memoize1`) | CPython both `[1, 99]` — **fixed TICKET-203** |
| C2 | P2 | `chezzi test --timeout=500`: a job faults at 0 ms, the test then sleeps 3 s | only `TIMED-OUT`; the job's index fault is dropped | Go: the goroutine panic ends the run |
| K4 | P2 | `p := pair[str, int]` | `expected ']', found ','` (the call form and one-arg `idt[int]` work) | Go accepts |
| A2 | P2 | `t.0.1` | `expected identifier, found float 0.1` | Rust `2` — **fixed in place 7e0581a3** |
| S1 | P3 | `print(b"'")` | `b'\''` | CPython `b"'"` — **fixed in place c0a4c4e5** |
| S2 | P3 | `json.decode` of a recursive struct | names `'s2::Node'` | bare name — **fixed in place c0a4c4e5** |
| S3 | P3 | field default `n: int = Self.mk()` | a second, false "filled by the callee ... END of a call" error on `A()` | only `unknown name 'Self'` |

Repros: `~/.cache/hunt4/{air,cancel,chan,check,std}/` (`chan/p/q1.chz`, `q11.chz`; `cancel/p/n5.chz`,
`t/y2_test.chz`; `check/final/K*.chz`; `air/t2.chz`, `m2.chz`).

## Family N3 — what a bare name denotes here (K6, K5, K1, S3)

**Fact:** which binding does this `Ident` denote at this program point? A module slot's content is
its last write (CPython).

**Deciders today.** Two answers exist. (A) slot/scope resolution — `owning_scope`/`head_binding`
(`checker/setup.rs:2652`, `:2666`), `GlobalBinding.decls` (`globals.rs:38`), `labels_certain`
(`globals.rs:277`), TICKET-196's `capture_sig` certain-fn rule (`setup.rs:2305`). (B) membership in
`self.functions`, which holds every hoisted `fn` and every from-imported certain fn and is never
cleared when a later top-level `:=` writes the same slot (`declare`, `setup.rs:2577`, untaints
`imported_values` only). (B) is read as "the slot holds this fn" by:
- `let_holds_one_known_fn` (`sig.rs:611`, seeds `kw_certain` at `:382`, `:2798`) — K6;
- `value_head_resolution` (`expr.rs:2092`, functions-first, before `Global`), feeding
  `resolution_diverges` -> `is_diverging_native` (`mod.rs:605`) — K5; and the direct-call arm
  `expr.rs:2943`;
- about 15 more reads (`sig.rs:367`, `:4096`, `expr.rs:732`, `:2803`, `pattern.rs:1958`, ...).
A third resolver, desugar's `check_param_defaults`/`default_referenced_name` (`desugar/mod.rs:783`,
struct copy `:750`), matches a default's Idents against param/field NAMES with no binders and no
notion of where the default is evaluated — K1. S3 is the same default machinery: a struct field
default classified `Dflt::CalleeFilled` reaches a ctor, which has no callee to fill it.

**Grid:** consumer {kw/labels through a value, divergence, default legality, resolution kind} x
binding {hoisted fn, fn then top-level `:=`, import then `:=`, local shadow, lambda param,
comprehension var, struct field}. Broken: exactly the cells where the bare name and the binding
disagree (fn-then-`:=` for labels; import-then-`:=` for divergence; every binder cell for defaults).

**History:** ~12 patches since TICKET-077 on these functions alone; w16 counts 22 earlier Names
patches. Total ~34.

**Wrong assumption:** "`n` is in `self.functions`" means "`n`'s slot holds that fn here", and "an
Ident spelled like a param in a default" means "it references the param".

**Single source:** one answer per Ident `NodeId` — the `Resolution` table TICKET-180/182 record
(`record_resolution`, `expr.rs:2010`), emitting `Fn{..}` only when the slot is certain (one
`Fn`/`Hoisted`/`Import` decl, the `capture_sig` rule; or before any rewrite, the `labels_certain`
rule). A redeclared slot resolves to `Global`. Then divergence (K5) follows the resolution,
`let_holds_one_known_fn`'s Ident arm becomes `labels_certain(n).ok()` (K6), and default legality
becomes "this default's Idents resolve in the scope the default is evaluated in" (K1: move the check
into the checker, binder-aware; the decl-site default typing at `expr.rs:153` must use the same
scope). `functions` stays a table of declarations, never consulted as slot content.

## Family G3 — when a bound's type arguments are resolved (K2, K3)

**Fact:** does type argument A satisfy bound `P[args]` here, with `args` meaning what they mean at
the declaration?

**Deciders today.** A bound's arguments are stored as unresolved AST (`Bound.args: Vec<Type>`,
`ast/mod.rs:548`) and resolved lazily inside `enforce_bounds` (`proto.rs:4448`) by
`resolve_bound_arg` (`sig.rs:5649`), which runs `resolve_type` in the CALLER's scope with only the
callee's own params entered (K3: the receiver's `T` becomes the caller's `T`, or `unknown type`).
`instantiate_method` (`mod.rs:3577`) substitutes the signature only (`subst_sig`, `mod.rs:3519`) and
never applies the receiver map to bounds. The yes/no is `bound_args_match` (`proto.rs:1516`):
`!ty_fully_concrete(&bt) || !ty_fully_concrete(want) || compatible(..)` — any abstract argument
passes (K2; added in c59418a8 as "anything still generic forwards loosely"). About 20 call sites of
`enforce_bounds` (`expr.rs:1492`, `:1501`, `:3616`, `proto.rs:5435`, `pattern.rs:2387`, ...) and
`satisfies_methods`' where-check (`proto.rs:2652`) inherit both.

**Grid:** bound site {fn call, where clause, static call, struct ctor, method own-param bound,
method receiver-param bound} x argument {concrete, caller param with matching bound, caller param
with mismatched bound, unbound param}. Broken: every "mismatched abstract" cell, and the whole
receiver-param row (false reject or capture).

**History:** `enforce_bounds` 35 commits; ~60 archive rows mention bounds (W8-44 and 4 follow-ups,
W8-45, W9-3, W7-41/45/53/54, M24-1..5); none touched lazy resolution or the loose match.

**Wrong assumption:** a bound can be resolved wherever it is checked, and an abstract argument
cannot be judged so it passes.

**Single source:** resolve bounds to `Ty` once, when the signature is built (the declaration's
scope, receiver params included), carry them in the signature, and substitute them with the same
map as the params in `subst_sig`/`instantiate_method` (so the TICKET-197 capture-avoiding rename
covers them). `enforce_bounds` compares instantiated `Ty`s; `bound_args_match` compares abstract
arguments by identity (`Param(X) == Param(X)`) and passes only on `Unknown`. K4 (multi-arg
turbofish as a value) is a neighbour in the same "type-argument application" area: the one-arg
value form goes through the checker's `Index{Ident, ..}` reinterpretation and the multi-arg form
only parses before `.member` (`parser/mod.rs:2787`) — two deciders of one form; folded into this
ticket.

## Family B3 — reaching every parked party on a cancel, and ranking at the end (C1, C2)

**Fact (C1):** when a scope's cancel trips, every parked member is drained — channel/`wait:` parks
from `parked` AND socket parks from the poller registry.

**Deciders today.** Six "wake a parked party on cancel" sites. Five do both halves by hand
(`cancel_drain` + `poller::drain_sched`/`drain_scope`): sibling-fault `finish` (`sched.rs:2484`),
`abort_enlisted_scope` (`:955`), `abort_eager_nursery` (`:1434`), `cancel_fiber_owned_family`
(`:6584`), `cancel_all` (`mod.rs:4564`). The sixth, TICKET-118's ancestor-trip scan
(`drain_scan_due` -> `cancelled_scope_awaiting_drain` -> `cancel_drain`, `mod.rs:2692`, `:3518`),
which is how `shutdown_now` reaches a job's nursery, copied only the `parked` half; its predicate
also walks only `parked`, while a socket-parked fiber is counted `inflight`. Sleep/timer/guard/join
need no waker (they poll `block_halt_check`), which is why only sockets hang.

**Fact (C2):** which cause a run reports at its end. `on_step_fault` (`exec.rs:1589`) swaps in an
unjoined job fault only for a deadlock verdict; `finish_run` (`netio.rs:4241`) drains only on
deadlock; the test reap upgrades only a `Pass` (`test_runner.rs:821`). There is no one end-of-run
ranking; `unwind_result` ranks a cause against its own cleanup only.

**Grid:** cancel source {sibling fault, shutdown_now, nested shutdown_now, exit, --timeout} x park
{recv/send, sleep, guard, socket accept/read/write} x owner {main nursery, job, job's nursery,
nested job's nursery}: broken = shutdown_now x socket x {job's nursery, nested job's nursery}.
Ranking: {job fault} x {deadlock ok, --timeout broken, --max-heap same gate, test's own fault
dropped, exit (DEC-181: exit wins)}.

**History:** ~12 tickets on the drain class (D6b, W7-16/18/39/57/59/60, TICKET-118/188/194/195);
TICKET-118 is the direct ancestor (its pin test used `recv`). ~3 on the ranking class.

**Wrong assumption:** "a cancelled fiber is in `parked`", and "only a deadlock can be outranked by an
unjoined job fault".

**Single source:** one `drain_family(sid)` that always does both halves, called by all six sites,
with a predicate that counts poll-parked fibers of a tripped scope. One end-of-run ranking (beside
`unwind_result`/`finish_run`) that every driver and the test reap use for every terminal cause.

## Family S1 — scheduler identity and fairness (H1, H2)

**H1 fact:** what a scope id names. A nursery opened inside a spawned task registers its scope on
the shared `SchedCore.scopes` Vec (`register_scope_seeded`, `mod.rs:2978`; id = `len()`), and
`join/abort_fiber_owned_nursery` (`sched.rs:1506`, `:1532`) call `retire_last_scope`
(`mod.rs:3159`), which POPS. `mn_worker_loop` (`sched.rs:2451`) copies `fiber.scope_id`, calls
`finish` (which wakes the owner at the join and drops the lock), then retakes the lock and calls
`cancel_drain(scope_id)`. In the gap another worker resumes the owner, which joins and pops: the id
is now out of bounds (the panic) or, worse, reused by the next nursery, whose fibers get drained
(silent structured-concurrency break). The same unlock-then-index gap is at `mod.rs:3518` and in
`cancel_all` (`:4558`). Wrong assumption: "scope ids are append-only and never shift" (comment at
`mod.rs:2925`). Single source: stable scope identity inside `SchedCore` (never reuse an index, or a
generation checked by every lookup), not a guard per call site.

**H2 fact:** every runnable fiber is reached within a bounded number of scheduling decisions. At T=1
only the drainer runs fibers. A cap-0 give puts the receiver in `runnext` (`hand_off`,
`mod.rs:4009`, from 3164b0e5) and a receive that takes an offer puts the sender there
(`handoff_wake`, `netio.rs:1944`); `LocalQ::pop` always takes `runnext` first (`mod.rs:2131`) and
the only fairness bound (every 61st tick, `mod.rs:3405`) pulls from the GLOBAL queue, so a fiber in
the local ring behind a permanently refilled `runnext` never runs. Wrong assumption: "the 61-tick
global pull bounds latency for every runnable fiber". Single source: `LocalQ::pop` owns fairness —
Go's `inheritTime`: a `runnext` fiber inherits the current slice, and after the slice the ring is
served.

**History:** `cancel_drain` 35 commits, `runnext` 27; `scope_family`/`retire_last_scope` from
TICKET-103, changed by 125; 132, 185 (17 commits); W17-1 is the open perf residual on the H2 path.

**Status:** H1 fixed by `ScopeTable` (TICKET-199): scope ids are never reissued, and a retired id
reads as absent. H2 fixed in `LocalQ::pop` (TICKET-199): a `runnext` pick is `Slice::Inherit` and
runs on the current slice. W17-1 re-measured: no effect.

## Isolated

- **A2** (fixed in place, 7e0581a3): the lexer read `0.1` after a field `.` as a float.
- **S1, S2** (fixed in place, c0a4c4e5): `bytes`/`bytearray` repr quote choice now shares
  `repr_quote` with `str_repr`; `json.decode`'s type errors name the bare type.
- **S3** (P3): folded into N3 (struct field defaults are classified by the same machinery).

## Owner decisions (2026-10-02)

1. **A1 is a bug, not a new decision.** TICKET-191's "a copy gets a snapshot" meant the value as of the
   crossing, like every other value a spawn captures: the copy must see `[1, 99]`. Today `get()` in a
   copy returns the fill-time value from the shared core and ignores the up-to-date object the copy
   already holds (`Task.cached`, memoize's `own` map).
2. **C2: an unjoined job fault that happened before a `--timeout` is reported** (with the job's frames),
   as Go's panic ends the run at the fault. The timeout wins only when no earlier fault exists.
3. **K1: accept `k := 1; fn f(k: int = k)`.** The default reads the global `k`, as CPython evaluates a
   default in the enclosing scope. Every binder (lambda params, comprehension vars) is scoped.

## Plan order

1. **S1** (H1 P0 crash, H2 hang) — TICKET-199 — scheduler; two parts, one ticket.
2. **B3** (C1, C2) — TICKET-200 — after S1 (both touch `mod.rs` drain paths).
3. **N3** (K6, K5, K1, S3) — TICKET-201 — checker names; independent of 1-2. **Fixed** by
   `Checker::slot_holds_fn_decl` and `Checker::check_default_scope`; grid `name_denotes_grid`.
4. **G3** (K2, K3) — TICKET-202; K4 split out to TICKET-204 (parser/AST, no shared code) — checker generics; after N3 if they share files.
5. **A1** — TICKET-203 — a bug against TICKET-191's decision; small (`std/concurrency` task + memoize).

## Owner decision (2026-10-02): what a type-applied name may be — follow Rust

TICKET-204's scope. Rust's rule: a PATH to a fn-like item is a fn value; a method bound to a receiver
is not. So these are values, at any type-arg arity, same-module, imported or module-qualified:
a generic fn (`g := pair[str, int]`, Rust `pair::<&str, i32>`), a payload enum variant
(`f := E[int].V`, Rust `E::<i32>::V`), a static method (`f := Box[int].make`, Rust
`Box::<i32>::make`), and a method reached through its TYPE, which takes `self` as an explicit first
argument (`g := Box[int].get; g(b)`, Rust `Box::<i32>::get`). A module path takes a multi-arg type
turbofish (`lib.R2[int, str].L(1)`), and a call with explicit type args takes named args
(`Box[int](v=1)`, Rust `Box::<i32>{v: 1}`). NOT values: a bound method `b.get` (Rust E0615: it would
hide its `self` capture — write `fn(): b.get()`), a named-field struct (`f := Box[int]`; Rust struct
literals are not fns), and a bare type. Each rejection must say so correctly; today `f := Box[int]`
reports "unknown type 'Box'; import it from lib" for an imported `Box`. A bare generic path with
nothing pinning its params stays the Go/Rust error.
