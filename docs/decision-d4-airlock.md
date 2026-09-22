# D4 — no silent copies at the task airlock (PROPOSED, 2026-09-22)

**Status: PROPOSED, awaiting owner approval.** If approved, it supersedes **D2** (DEC-137, TICKET-137:
"a received closure reads the running task's module globals"). Until then D2 stands.

## The problem

A task gets its own deep copy of everything it captures, and of every module global. That is the
airlock. A write inside the task lands in that copy and is lost at the join. The language accepts the
write without complaint, so a program that looks like Go or Python quietly prints the old value.

Every row below was measured on the release binary at `b3a72263`. The Go, CPython and Rust columns
are run twins of the same program.

| # | shape | Chezzi | diagnostic | Go 1.27 | CPython 3.14 | Rust 1.98 |
|---|---|---|---|---|---|---|
| 1 | `n := 1` · `spawn: n = 5` · `print(n)` | `1` | warning | `5` | `5` | move closure: `1` **plus a warning** (`value assigned to n is never read`) |
| 2 | `xs := [1]` · `spawn: xs.push(2)` · `print(xs)` | `[1]` | warning | `[1 2]` | `[1, 2]` | scoped thread: `[1, 2]`; move closure: **compile error** if `xs` is used after the move |
| 3 | global `n := 1` · `fn bump(): n = 5` · `spawn: bump()` · `print(n)` | `1` | **none** | `5` | `5` | `static mut` needs `unsafe`; a safe global needs `Mutex`/`Atomic` |
| 4 | global `xs := [1]` · `fn bump(): xs.push(2)` · `spawn bump()` · `print(xs)` | `[1]` | **none** | `[1 2]` | `[1, 2]` | same as 3 |
| 5 | `f := fn(): xs.push(2)` · `spawn: f()` · `print(xs)` | `[1]` | **none** | `[1 2]` | `[1, 2]` | an `FnMut` borrowing `xs` cannot be `spawn`ed without `move`, and then `xs` is moved away |
| 6 | W12-5 G6: a sent closure pushes to a capture aliasing `gl[0]` | `[1, 2] [1]` | none (D2 says this is correct) | `[1 2] [1 2]` | `[1, 2] [1, 2]` | — |

None of the three ancestors ever shows a stale copy silently. Go and Python share the value. Rust
either shares it (a scoped borrow), moves it so the stale name cannot be used again, or copies a `Copy`
value and warns. Chezzi is the only one of the four that copies, keeps the old name usable, and says
nothing in rows 3–6.

This one design choice is behind a large share of the airlock rows in the ledger: W8-3, W8-25,
W11-13, W11-15, W12-5, W13-1 and TICKET-165. Each fix covered one more shape, and the warning channel
still cannot see rows 3–5, because the write happens in a function or closure the checker does not
follow from the `spawn:`.

## Options considered

| option | what changes | verdict |
|---|---|---|
| A. keep D2 | nothing | rows 3–6 stay silent; more airlock rows every sweep |
| B. reverse D2 (TICKET-154's withdrawn fix) | row 6 matches Go | fixes one shape; rows 3–5 stay silent |
| C. share memory like Go | rows 1–6 match Go | needs one shared, thread-safe GC heap: a rewrite of the value model, GC and scheduler. Not before the JIT |
| **D. make every lost write an error** | rows 1–6 are rejected, at compile time where the checker can see the write and at runtime where it cannot | **recommended** |

## The rule (D4)

A task may READ what it captured and the module globals. It may not WRITE a copy it did not create.
Real sharing goes through the handle types, which already exist and already work: `Shared`,
`RwShared`, `Atomic`/`AtomicInt`, `Channel`. This is Rust's rule, and Rust owns errors and control
flow in Chezzi's lineage. It also matches the model `docs/concurrency.md` already documents ("per-task
isolation; the handles are the only real sharing"): D4 does not change what the airlock does, only
the fact that a write lost to it is silent.

1. **Compile-time error: a write to a captured binding inside a task body.** This covers rows 1–2 and
   every projected form TICKET-165 now tracks (`s.v = 2`, `xs[0] = v`, `xs[0].push(2)`, `m[k] += 1`).
   The positions are a `spawn:` block, a `spawn f(...)` argument expression, and a `defer:` inside a
   task. The message names the fix: `cannot write 'xs' inside spawn: the task has its own copy and the
   write would be lost at the join; share it through Shared/Channel, or declare a task-local with :=`.
   It replaces the W8-3/W11-13 warning, which becomes redundant.
2. **Runtime fault: a spawned task writes a module global.** This covers rows 3–4. The checker cannot
   see these writes, because they happen inside an ordinary `fn` that is also legal on the main task.
   The runtime can: the task knows it is not the root task. Both a global store (`n = 5`) and a
   mutation of the task's copy of a global's object (`xs.push(2)`) fault `module global 'xs' is
   per-task: a spawned task cannot write it; share it through Shared/Channel`, catchable with
   `recover:`. The main task keeps full read/write access to its globals, so single-threaded scripts
   are unaffected. Where the checker can see a global write directly in a task body, it reports it at
   compile time as in rule 1.
3. **A closure that writes a captured binding cannot cross a task boundary.** This covers rows 5–6.
   Crossing means being sent on a `Channel`, captured by a `spawn:`, or passed to `spawn f(...)` or an
   `Executor`. Where the checker knows the closure, this is a compile-time error. Otherwise it is a
   runtime fault at the airlock, since `ensure_crossable` already walks every crossing closure. A
   closure that only reads its captures still crosses by value, as today.
4. **D2 becomes moot.** Rules 2 and 3 mean no task can have written the globals or captures that a
   crossing closure reads. The only divergence left is a write by the MAIN task after it spawned the
   reader, which is the ordinary "a task sees the globals as they were at its spawn" snapshot. That is
   already documented (`module_global_freshness_test.chz`) and is a read of consistent data, never a
   lost write. D2's rule text stays, as the definition of which snapshot a received closure reads.

## Measured cost

An instrumented checker (a scratch clone, never committed) printed every site rule 1 or rule 2 would
reject. It ran over all 436 `.chz` files in `tests/chz`, `examples`, `benches` and `std`.

| site | count | where |
|---|---|---|
| write to a captured binding in a task body (rule 1) | **30** | all in 4 airlock test files that pin today's copy behaviour (`airlock_alias_identity_test`, `airlock_alias_root_gaps_test`, `airlock_received_closure_globals_test`, `module_global_freshness_test`), plus the teaching example `examples/capture_spawn_isolated.chz` (4) |
| fn-body write to a module global, in a file that spawns (rule 2 candidates) | **33** | 32 in airlock/global-isolation test files (`module_global_freshness_test` 18, `airlock_alias_root_gaps_test` 10, `airlock_global_free_gaps_test` 3, `airlock_shared_binding_test` 1); **1 real use**: `fn_value_tuple_slot_call_test` spawns a helper that bumps a global counter (`w1216_hits = w1216_hits + n`) and returns it, which would fault under rule 2 and moves to `AtomicInt` |
| fn-body write to a module global in a file that never spawns | 25 | NOT affected: these run on the main task (`eq_protocol_*`, `list_test`, `examples/list_hof_shrink.chz`, …) |
| `benches/`, `std/` | **0** | — |

So the migration is limited to the tests that pin the behaviour D4 removes. They are rewritten as
`rejects` checker tests and `recover:` fault tests. Plus one teaching example, which becomes the
"use `Shared`" example. Only one non-airlock program in the corpus writes a task-side copy, the `fn_value_tuple_slot_call_test` counter above, and it reads the value back inside the same task. No program writes a copy and then reads the parent's side on purpose.

Not measured yet: rule 3's sites (closures that write their captures and then cross). The checker
cannot enumerate them without the rule itself. The implementing ticket must measure them before
committing to compile-time rejection for that rule.

## Risks and open questions for the implementing ticket

- **Rule 2's runtime cost.** A global store in a spawned task is one branch on a per-task flag. The
  object-mutation half (`xs.push` on a global's copy) needs the task's global copies marked, for
  example one header bit set at the airlock, and a check on every mutating op. That is a hot path.
  Measure `benches/run.chz` on release. If it is not level, the fallback is to fault only at the
  global-store opcode plus at mutations reached through a global LOAD in a task, which is a
  per-load-site check.
- **Rule 3 in first-class positions.** A closure stored in a `List` and sent inside it is crossed by
  `ensure_crossable`'s walk, so the runtime half covers it. The compile-time half is best-effort and
  must decline when unsure (the CLAUDE.md warning-gate convention applies to errors too: never reject
  a program that would have been correct).
- **`Executor` jobs** are tasks for rules 1–3.
- **Error, not warning, at compile time.** Rust makes this an error. A warning is what exists today,
  and rows 3–6 show that a warning which cannot see every site does not stop the bug class.
- **Soundness check before landing.** Run the TICKET-167 seed sweep and the CPython differential.
  Every rewritten airlock test must show its old expectation now rejected or faulting, on the
  pre-change binary as well as the post-change one.

## If approved

1. Owner records the decision (supersedes D2's "not a bug" status for W12-5; W12-5 G6 closes as
   "rejected by D4 rule 3").
2. File one ticket for rules 1+2 (checker + runtime) and one for rule 3, sequenced after TICKET-167/168.
3. `docs/concurrency.md`'s isolation section, `docs/syntax.md`'s warning table, and `CLAUDE.md`'s
   warning-channel list are updated in those tickets.
