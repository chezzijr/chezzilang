# D4 — no silent copies at the task airlock (APPROVED, 2026-09-22)

**Status: APPROVED 2026-09-22; layers C and A LANDED in TICKET-169 and TICKET-170.** It supersedes **D2** (DEC-137, TICKET-137) as the answer
to "is a lost task-side write correct": it is not, and it faults. D2's rule survives only as the
definition of WHICH snapshot of the globals a received closure reads. Detection design: a runtime copy
mark (C) plus checker inference (A). Implementation: TICKET-169 (layer C, runtime: every rule) and
TICKET-170 (layer A, checker: the early compile-time errors). Both are sequenced after TICKET-167/168.

Layer C landed in TICKET-169. Layer A landed in TICKET-170.

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
   The positions are a `spawn:` block and a `defer:` inside a task. A `spawn f(...)` argument
   expression runs in the parent and is not a task position. The message is layer C's exact text:
   `'xs' is this task's copy: a write to it would be lost at the join; share it through
   Shared/Channel, or make a task-local copy with .copy()`.
   It replaces the W8-3/W11-13 warning, which becomes redundant.
2. **Runtime fault: a spawned task writes a module global.** This covers rows 3–4. The checker cannot
   see these writes, because they happen inside an ordinary `fn` that is also legal on the main task.
   The runtime can: the task knows it is not the root task. Both a global store (`n = 5`) and a
   mutation of the task's copy of a global's object (`xs.push(2)`) fault `module global 'xs' is
   per-task: a spawned task cannot write it; share it through Shared/Channel`, catchable with
   `recover:`. The main task keeps full read/write access to its globals, so single-threaded scripts
   are unaffected. Where the checker can see a global write directly in a task body, it reports it at
   compile time as in rule 1.
3. **A closure that writes a captured binding cannot execute across a task boundary.** This covers
   rows 5–6. Layer A rejects a direct captured call inside `spawn:`, `spawn g()`, and
   `Executor.submit(g)` where it knows the literal. It declines `Channel.send(g)` and a closure passed
   as a `spawn f(...)` argument because neither crossing proves execution. Layer C covers every
   crossing closure. A closure that only reads its captures still crosses by value, as today.
4. **D2 becomes moot.** Rules 2 and 3 mean no task can have written the globals or captures that a
   crossing closure reads. The only divergence left is a write by the MAIN task after it spawned the
   reader, which is the ordinary "a task sees the globals as they were at its spawn" snapshot. That is
   already documented (`module_global_freshness_test.chz`) and is a read of consistent data, never a
   lost write. D2's rule text stays, as the definition of which snapshot a received closure reads.

## How a write is detected: a runtime mark (C) plus checker inference (A)

The checker cannot know by itself whether `xs.method()` writes. Today it has a hand-written list for
the built-in types only (`mutates_receiver`, `src/checker/mod.rs`: `List.push/pop/sort/...`,
`Map.remove/update`, `Set.add/remove`). A user method is invisible to it. Measured on the release
binary at `b3a72263`:

    struct C:
        n: int
        fn bump(self):
            self.n = self.n + 1
    fn main():
        c := C(1)
        parallel:
            spawn:
                c.bump()
        print(c.n)          # 1, with no warning at all
    main()

So D4 detects a write in two layers:

- **C: the runtime mark is the rule.** Every object the airlock copies into a task (captures and
  module globals) carries a "copied" bit in its header. Any write to a marked object faults, at the
  write itself: a field store, an index store, or a mutating native (`push`, `insert`, `Map` store,
  `Set.add`, ...). The fault is `'c' is this task's copy: a write to it would be lost at the join;
  share it through Shared/Channel, or make a task-local copy with .copy()`. Because it catches the
  write where it actually happens, it covers built-in methods, user methods, protocol and generic
  dispatch, closures and fn values alike, with no inference and no new syntax. Reads never check the
  bit, so a read-only task pays nothing. An object the task creates itself (`ys := xs.copy()`, a new
  list) is unmarked and freely writable. The mark is on the task's COPY; the parent's original is
  never marked, which is the difference from Kotlin/Native's old model below.
- **A: checker inference is the early error.** The checker infers, per method, whether it writes
  `self`. A method does if its body writes `self.f`/`self[i]`, or calls a known-writing method on
  `self` or on one of its fields; this is a fixed point over the call graph, the same kind of
  inference as return types (no `mut self` annotation). With that it reports rules 1 and 3 at compile
  time for direct calls. Where it cannot know (a protocol or generic `T` receiver, a fn value, a
  closure it did not see), it DECLINES and leaves the write to layer C. Per the CLAUDE.md
  warning-gate convention, it must never reject a program that would not have faulted at runtime.
- **Rejected: B, an explicit `mut self` (Rust's `&mut self`, Pony's reference capabilities).** It is
  complete at compile time, but it adds syntax and boilerplate on every method and changes every
  protocol's signature. C gives the same completeness at runtime without it.

## Reference languages

Measured where the runtime is installed (Ruby 3.4.10, Node 26.8.1); documented behaviour otherwise.

| language | what happens | measured / source | relation to D4 |
|---|---|---|---|
| **Ruby Ractor** | a non-main Ractor touching a global raises; writing a frozen object raises; a plain (unfrozen) object passed in is deep-COPIED and the copy is freely writable | `Ractor::IsolationError: can not access global variables $g from non-main Ractors`; `FrozenError: can't modify frozen Array: [1]`; unfrozen `ys << 2` inside gives `[1, 2]` while the parent keeps `[1]`, silently | the closest precedent. Rule 2 is Ruby's global rule, and C is Ruby's `FrozenError` applied to the copy. Ruby's third case is exactly Chezzi's bug today, and it is the case D4 closes |
| **Kotlin/Native** (memory model before 1.7.20) | objects shared between Workers were FROZEN; a write threw `InvalidMutabilityException` at runtime | documented; removed in the 2022 memory model | proves C is implementable, and is a warning: users hated it, because freezing was transitive and hit the ORIGINAL object too, so a value became read-only in the thread that owned it. D4 marks only the task's copy, never the parent's object |
| **Swift 6** strict concurrency | a global mutable `var` used from concurrent code is a compile error, `... is not concurrency-safe because it is nonisolated global shared mutable state`; values crossing actors must be `Sendable` | documented | rule 2 at compile time, where Swift's global isolation makes it decidable; Chezzi needs the runtime half because its fns are not isolation-annotated |
| **Rust** | `static mut` needs `unsafe`; `&mut` captures cannot cross `thread::spawn` without `move`, after which the original is unusable | measured (rustc 1.98.1, table above) | layer A's ideal, obtained through annotation (B), which D4 rejects |
| **JavaScript** | Web Workers structured-clone the message (a silent copy, like Chezzi today); `Object.freeze` + strict mode throws on write | `TypeError: Cannot assign to read only property 'n' of object` | copy + freeze are the two halves; JS never joins them automatically |
| **Erlang/Elixir** | all data is immutable; a message is a copy that cannot be written | documented | the fully strict end: no mutable data at all |
| **Go** | shares memory; a racing write is a data race, found at runtime only by `-race` | measured (table above) | what Chezzi's syntax looks like; D4 deliberately does not copy it (option C in the table above) |

No language does exactly C + A. Ruby Ractor is the nearest: it has the global rule and the frozen
write-fault, but it leaves the unfrozen-copy case silent. D4's contribution is marking the copy
automatically at the airlock, so that case faults too.

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

TICKET-170 measured rule 3 before enabling it: two corpus sites, both direct callee positions
(`airlock_task_local_fault_test.chz` and `examples/capture_spawn_closure_mutates_isolated.chz`). The
full measurement found 20 rule-1 sites across six files and one rule-2 method call. All were fault
pins or examples and were migrated through declined helper-call shapes. A closure passed as a
`spawn f(...)` argument is not executed by the boundary; measured `spawn run(g)` stayed clean when
`run` never called `g`, so layer A declines that shape.

## Risks and open questions for the implementing ticket

- **The runtime mark's cost (layer C).** Every write site (field store, index store, each mutating
  native) gains one header-bit test, which is a hot path. Measure `benches/run.chz` and the
  struct/list-heavy benches on the release binary. The header must have a spare bit, or the `Value`
  size must not grow (see `docs/future.md` §4, the NaN-boxing plan). If it is not level, the fallback is
  to mark only at the airlock's root objects and check at global-store opcodes and at mutating calls
  whose receiver came from a capture or global load, which is a narrower per-site check.
- **Deep vs shallow mark.** The airlock deep-copies, so every object in the copied graph is marked,
  not just the root. `xs[0].push(2)` must fault as well as `xs.push(2)`. The mark is set in the same
  walk that copies, so it costs no extra traversal.
- **Layer A's fixed point** runs over user methods only. A native method's write set is its
  `mutates_receiver` entry (extended to every mutating native, with a drift test like
  `builtin_method_slices_all_resolve`).
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
