# D4 — no silent copies at the task airlock

**Status: accepted 2026-09-22, implemented.** Layer C (runtime, every rule) in TICKET-169; layer A
(checker, the early compile-time errors) in TICKET-170; refinements in TICKET-179, -189, -190, -220,
-240, -247 and -254. D4 supersedes **D2** (DEC-137, TICKET-137) as the answer to "is a lost task-side
write correct": it is not, and it faults. D2 survives only as the definition of WHICH snapshot of the
globals a received closure reads. The full approval record, with the measured corpus cost and the
pre-landing risk list, is kept verbatim in
[`history/designs/adr-d4-airlock-full.md`](../history/designs/adr-d4-airlock-full.md). The user rules
are in [`concurrency.md`](../reference/concurrency/01-model.md).

## The problem

A task gets its own deep copy of everything it captures, and of every module global. A write inside
the task lands in that copy and is lost at the join. Before D4 the language accepted the write and a
program that looks like Go or Python printed the old value. Measured at `b3a72263`:

| # | shape | Chezzi before D4 | Go 1.27 | CPython 3.14 | Rust 1.98 |
|---|---|---|---|---|---|
| 1 | `n := 1` · `spawn: n = 5` · `print(n)` | `1`, warning | `5` | `5` | move closure: `1` plus a warning |
| 2 | `xs := [1]` · `spawn: xs.push(2)` · `print(xs)` | `[1]`, warning | `[1 2]` | `[1, 2]` | scoped: `[1, 2]`; move: compile error if `xs` is used after |
| 3 | global `n := 1` · `fn bump(): n = 5` · `spawn: bump()` | `1`, **silent** | `5` | `5` | `static mut` needs `unsafe` |
| 4 | global `xs := [1]` · `fn bump(): xs.push(2)` · `spawn bump()` | `[1]`, **silent** | `[1 2]` | `[1, 2]` | same as 3 |
| 5 | `f := fn(): xs.push(2)` · `spawn: f()` | `[1]`, **silent** | `[1 2]` | `[1, 2]` | an `FnMut` borrow cannot be spawned without `move` |
| 6 | W12-5 G6: a sent closure pushes to a capture aliasing `gl[0]` | `[1, 2] [1]`, silent | `[1 2] [1 2]` | `[1, 2] [1, 2]` | — |

No ancestor shows a stale copy silently. This one choice was behind W8-3, W8-25, W11-13, W11-15,
W12-5, W13-1 and TICKET-165.

## Decision

A task may READ what it captured and the module globals. It may not WRITE a copy it did not create.
Real sharing goes through the handle types: `Shared`, `RwShared`, `Atomic`/`AtomicInt`, `Channel`.
This is Rust's rule. D4 does not change what the airlock does; it makes a lost write an error.

**Rejected alternatives:**

- **A. Keep D2:** rows 3-6 stay silent, and every sweep finds more airlock rows.
- **B. Reverse D2** (TICKET-154's withdrawn fix): fixes row 6 only.
- **C. Share memory like Go:** needs one shared thread-safe GC heap, a rewrite of the value model,
  GC and scheduler. Not before the JIT.
- **Explicit `mut self`** (Rust `&mut self`, Pony capabilities): complete at compile time, but adds
  syntax to every method and changes every protocol signature. The runtime mark gives the same
  completeness.

## The rules

1. **Compile-time error: a write to a captured binding inside a task body.** Covers rows 1-2 and every
   projected form (`s.v = 2`, `xs[0] = v`, `xs[0].push(2)`, `m[k] += 1`). Task positions: a `spawn:`
   block and a `defer:` inside a task. A `spawn f(...)` argument runs in the parent and is not a task
   position. Message: `'xs' is this task's copy: a write to it would be lost at the join; share it
   through Shared/Channel, or make a task-local copy with .copy()`. It replaces the W8-3/W11-13
   warning.
2. **Runtime fault: a spawned task writes a module global.** Covers rows 3-4, where the write sits in
   an ordinary `fn` that is also legal on the main task. A global store (`n = 5`) and a mutation of
   the task's copy of a global's object (`xs.push(2)`) both fault `module global 'xs' is per-task: a
   spawned task cannot write it; share it through Shared/Channel`, catchable with `recover:`. The main
   task keeps full access. Where the checker sees a global write directly in a task body, rule 1
   reports it at compile time.
3. **A closure that writes a captured binding cannot execute across a task boundary.** Covers rows
   5-6. Layer A rejects a direct captured call inside `spawn:`, `spawn g()`, and `Executor.submit(g)`
   with a known literal. It declines `Channel.send(g)` and a closure passed as a `spawn f(...)`
   argument, because neither proves execution. Layer C covers every crossing closure. A closure that
   only reads its captures still crosses by value.
4. **D2 becomes moot.** The only divergence left is a write by the MAIN task after it spawned the
   reader: the task sees the globals as they were at its spawn. That is a read of consistent data,
   never a lost write.

**A builtin slice is no place link (TICKET-254).** A write whose LAST link is a slice of a `List`,
`str`, `bytes` or `bytearray` changes a new container: no D4 error, and the one warning
`'push' changes a temporary copy; the result is dropped: bind the value to a name first, or call 'push' on the source`
(`temporary_write`, `src/checker/sig.rs`). A write that goes on through an element
(`g[0..2][0].push(9)`) or a `Map[Range, V]` key still reaches the root. The write walkers
(`src/checker/fn_writes.rs`, `src/checker/self_writes.rs`) read syntax only; the checker decides which
subscripts are builtin slices (`range_subscript` → `Checker::fresh_subscripts`) and the summaries run
a second time with each link resolved. Do not add a syntactic "is this a range" test to a walker: it
is wrong in both directions.

```
fn t(grid: List[List[int]]):
    grid[0..2].push([9])

grid := [[1], [2], [3]]
parallel:
    spawn t(grid)
print(grid)
```

```
warning (d4.chz:2:5): 'push' changes a temporary copy; the result is dropped: bind the value to a name first, or call 'push' on the source
[[1], [2], [3]]
```

## How a write is detected

**Layer C, the runtime mark, is the rule.** Every object the airlock copies into a task carries a
"copied" bit. Any write to a marked object faults at the write: a field store, an index store, or a
mutating native. It covers built-in methods, user methods, protocol and generic dispatch, closures
and fn values with no inference. Reads never check the bit. An object the task creates is unmarked.
The mark is on the task's COPY only, never on the parent's original. The built-in mutator table is
`is_mutating_native_kind` (`src/vm/call.rs`); `Map.merge` returns a new map and is not a write.
`checker::tests::receiver_write_table_matches_observed_behavior` fails when the table disagrees with
observed behaviour.

**Layer A, checker inference, is the early error.** A fixed point over direct stores, typed mutators
and statically named calls infers writes through `self`, parameters, captures and globals. It reports
a write only when EVERY path reaches it: nothing under a branch, a loop body, the right side of
`and`/`or`/`??`, or after a possible early exit. Per the warning-gate convention it never rejects a
program that would not fault at runtime. Parameters map to sources through the call plan
(`bound_slots`), never by position. Indirect calls, protocol or generic receivers, and opaque closures
decline to layer C.

### What the mark means: "can the parent observe it after the join"

1. **A fresh value graph crosses unmarked (TICKET-240).** `Checker::fresh_shape` judges the operand
   once: a literal, comprehension, constructor, carrier wrap, inline literal default, variadic pack,
   or a container `.copy()` (for its own level; `copy()` is shallow) is fresh, child by child. A name,
   field read, index or call result stays marked. The compiler stores the shapes in
   `Program.fresh_calls`; `Vm::unmark_fresh` unmarks exactly what the shape names. A map is
   all-fresh only when every key and value is. Ceilings (each a false fault, never a lost write): a
   call result; a struct `.copy()`; a provider default; a shape past 16 levels or 64 positional
   children; a spawn table index past 31 bits.
2. **A crossing generator marks only what the parent can reach in its frame (TICKET-190, -240).**
   Each frame slot has one static verdict, `Marked`, `Root` or `All` (`vm::crossing::frame_slot`).
   A marking route (spawn forms, Executor jobs, the module snapshot, `Route::CopyRead`) rebuilds the
   frame marked, then unmarks each `Root` slot's root and each `All` slot's whole graph. A hand-off
   (Channel, Shared, RwShared, Atomic) marks nothing. `?x` and `!x` hold `x` (`aliases_operand`,
   `src/checker/fn_writes.rs`). The checker decides `Root` (`root_escapes`) and `All`
   (`Checker::deep_private`) once per generator decl; a param slot takes its shape from the creating
   call (`Op::StampGen`). Ceilings: a generator created through a method or fn value gets no stamp; a
   param at index 32+ and a local at slot 64+ stay marked; a comprehension-built local stays `Marked`.

## Reference languages

| language | behaviour | relation |
|---|---|---|
| Ruby Ractor | global access from a non-main Ractor raises; a frozen write raises; an unfrozen copy is silently writable | nearest precedent; D4 closes Ruby's silent case |
| Kotlin/Native (pre-1.7.20) | shared objects frozen, write throws | proves C works; froze the original too, which users hated; D4 marks only the copy |
| Swift 6 | a global mutable `var` from concurrent code is a compile error | rule 2 at compile time |
| Rust | `static mut` needs `unsafe`; `&mut` captures cannot cross without `move` | layer A's ideal, via annotation (rejected) |
| JavaScript | structured clone (silent copy); `Object.freeze` throws in strict mode | copy and freeze never joined |
| Erlang/Elixir | all data immutable | the strict end |
| Go | shares memory; races found only by `-race` | option C, rejected |
