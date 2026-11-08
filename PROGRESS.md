# Chezzi progress tracker

Chezzi is a fast, statically typed scripting language with a Python feel, built by hand in Rust.

This file is the single source of truth for "what is next". It holds the current state only.
Finished work is in git (`git log`) and in this month's dated log.

## How to update this file

- Write each new dated entry at the top of `docs/history/progress/<YYYY-MM>.md` for the current
  month. Create the month file when the month starts. Newest entries go first.
- Change this file only when the phase, the focus, the next steps or the ledger count change.
- Keep this file at 300 lines or fewer. Delete a finished item; git keeps it.
- Re-derive the open-row count after every merge (see [Ledger](#ledger)).

## Where things stand

The core language is implemented through M24 and still evolves:

- scalars, `List`/`Map`/`Set`/`tuple`, generic structs and enums;
- `T!E` / `T?` with `?`, `!e`, `?x` and implicit wrap; `None` is a keyword;
- generics with structural protocols, user `==` through `Eq`, static protocol requirements through
  witness passing;
- exhaustive `match` with guards, closures and higher-order functions;
- modules, including Python full paths (`a.b.X`);
- GC, interpolation, `defer`, `recover:`, `Iterator[T]`, slicing and indexing protocols, ranges
  as values.

Concurrency is complete through Tier-D: `spawn`, the `parallel:` nursery, `Channel[T]`,
`Broadcast[T]` (`std.concurrency.broadcast`), `Shared[T]`, `Executor`, the OS-thread M:N engine,
the netpoller and `std.net`. There is one engine: the bytecode VM on its M:N scheduler. Tasks share
one heap and get values by handle, as in Go
([`01-model.md`](docs/reference/concurrency/01-model.md)).

A wait inside a callback parks (a fiber) or is judged (`main`), as in Go. A hook, an operator, an
f-string part, `print`'s `str`, a HOF callback, a sort comparator, a `from_iter` drain, a
`Shared`/`RwShared` closure and a generator body run as frames with a `RetAction`
(`src/vm/ret_action.rs`). A `defer` drain is a trampoline frame (`src/vm/defer_drain.rs`). Only the
synchronous hard-halt drains (`--max-heap`, `--timeout`) keep the `deferring` exception in
`BlockCtx::judged`. The nested container walks, map/set probes, `json.decode` defaults and FFI
callbacks still re-enter (`gaps.md` W23-1, narrowed). Grid: `tests/callback_park_grid.rs`.
A guard wait and `Executor.shutdown()` park too, and a guard holder is preempted: every op of
`tests/blocking_context_grid.rs` but `stdin` runs with `demotes=0`.

The checker has a non-fatal warning channel (`Severity::Warning`, `"severity"` in `--errors=json`,
`DiagnosticSeverity::WARNING` in the LSP). It has five rules:

1. a discarded `T!E` / `T?`, or an unused std `Task` (TICKET-289);
2. a mutating call or a store on a temporary copy (a builtin slice, `.copy()`, an operator result);
3. an unreachable `match` arm;
4. an unread local;
5. a near-miss hook that its operator skips.

Test totals change with each ticket. Read the live counts from `cargo test` and
`chezzi test tests/chz`, not from prose.

## Current phase: root-cause refactor before the JIT

Bug-hunt waves 16 to 23 (JIT sweeps #1 to #8) were not clean. Their findings traced to a few
families (names, blocking, declarations, identity) where one fact had several deciders kept in sync
by hand.

**Rule: derive, don't mirror.** Each fact has one owner, and other code reads it. A fix deletes a
duplicate or adds a single source. See [`docs/contributing/lessons.md`](docs/contributing/lessons.md) §1.

The owner index is [`docs/contributing/owners.md`](docs/contributing/owners.md): it names the one
owner of each fact and lists each remaining copy as DUPLICATE. Done: the owner index, one topic per
file (`tests/source_layout.rs`), facts 1-9, builtin dispatch by id, one redeclaration rule, the
workspace crates, the blocking-point table (`block::wait_steps`) and the boundary-block table
(`boundary::RULES`).

## What is next

1. **TICKET-266, a `never` return type.** `exit` and `panic` declare `-> never`, and a call of that
   type leaves its block (Rust `-> !`). It deletes the `panic("unreachable")` lines after `os.exit`.
   Escalated; branch `ticket/266` kept.
2. **U5 sibling gaps** (the rest of TICKET-265, group C, builtin types and protocols).
3. **Grammar vs parser** (`owners.md` DUPLICATE, group F row 25): waits on G5.
4. **Std check and compile caching** (item 6 of the old list). Std modules are parsed at build time
   (`crates/chezzi-native/build.rs`). A std import still costs about 2.5 ms over `print(1)`
   (graph build, check, compile). The reach set (`resolver::unreached_std_fns`) and the program-wide
   slot and type keys depend on the importing program.
5. **A job parked in a stuck `defer` after a nursery child fault**, with a cut sleeping body, is not
   reported as a deadlock (base too).
6. **A cancelled task whose `defer` waits on a socket that never fires hangs**: a poll park is
   `inflight` and never judged (Go never reports a netpoll waiter as deadlocked either).
7. **Branch-join seed questions.** `infer_match` hides the `None` sibling slot under a seed:
   `b: bool = (match ...: 5 / None) == o` rejects `branches have incompatible types: int and None`.
   Reading the slot makes `fn f(o: int?) -> int?: return o ?? None` accept (cell `coalesce_none_arm`
   pins the reject). Owner question: accept it? Rust accepts `o.or(None)`. Also
   `a: Any = 5; o: int? = 5; print(a == o)` prints `false`.
8. **`io.read_line` faults on invalid UTF-8 at EOF** (`std/io.chz`, `src/native/io.rs`); it should
   give an error, as `b.decode()` does.
9. **Flaky floor:** `threads_one_serializes_cpu_bound_{callback,parallel}_tasks`
   (`tests/chezzi_threads_cli.rs`) trip their "finished too fast" floor at times. `burn(750000)` at
   T=1 is bimodal across processes; with ASLR off (`setarch -R`) it is not. `child_rusage::burn_scale`
   calibrates in one process and the test measures in another.
10. **Native defaults, review lows.** The native fn-value cache key `FnKey::Native(name, ptr)`
    (`src/vm/exec.rs`) has no module; `native_decl_defaults` (`src/base/builtin_method.rs`) returns
    `DeclDefaults::NONE` when a defaulted decl finds no row, where it should fail;
    `print(1, sep=1)` names "argument 2", not `sep`. `print(1)` is +1.9% instructions over
    `5e4033b6`, so the next prelude decl will probably cross the 2% line.
11. **Go `test/chan` ports.** 17 of 20 files in `tests/chz/go/`; select3 has 15 of its 24 cases (the 9
    left are always-block cases). Not ported: select5 (a generator), perm (directional channels),
    select2's heap check.
12. **`Set.map` / `Set.filter`:** later.
13. **Bug-hunt sweeps** resume after the refactor. They count toward the JIT entry rule below.
14. **`fs.glob` matches a wildcard only in the last path part** (`src/native/fs.rs` `glob`):
    `fs.glob("judge/problems/*/meta.toml")` returns `[]`. Go `filepath.Glob` and Python
    `glob.glob` both match it, and `05-native-system.md` promises the Go dialect.

Standing owner decision (2026-11-02): `benches/sched/extern_abs` may run slower than base (5-14%
measured, accepted for that cell only); every other bench cell keeps the 10% gate.

### Open questions for the owner

- **Copy protocol: deep (Rust `Clone`) or shallow (Python `copy`)?** Open since 2026-11-03. The
  candidate form is `protocol Copy: fn copy(self) -> Self`. Builtins would get intrinsic rows; a
  struct or enum would derive it when all its fields are `Copy`; `Channel`, `Shared`, `Executor`
  and `Task` would have no row. It would delete the 2 name-keyed `"copy"` checks
  (`checker/method_call.rs`, `vm/method_call.rs`). Enum `.copy()` waits for it.

### Owner decisions not yet implemented

- **U16: `Sized` and `Extend` protocols not declared yet** (owner decision 2026-10-16).
  A protocol only where generic code needs one; a new protocol goes in a std module, not the global
  namespace.

## JIT entry rule

Owner decision, 2026-09-22. The bug-hunt phase ends and the JIT starts (Tier 2 quickening and Tier 3
Cranelift, [`docs/design/roadmap.md`](docs/design/roadmap.md) §4) when all four conditions hold. The
ledger does not have to be empty.

| # | condition | status |
|---|---|---|
| 1 | The seeded scheduler oracle is built and re-finds reverted historical races (TICKET-167) | built; it re-finds 2 of 3 reverted races. The owner decides if that is enough. See [`docs/contributing/bug-discovery.md`](docs/contributing/bug-discovery.md) "Seeded scheduler oracle" |
| 2 | Two consecutive bug-hunt sweeps with zero new P0/P1 in the core: lexer, parser, checker, compiler, VM exec/call/arith/stmt, scheduler, GC, value model | count is 0. Sweeps #1 to #8 (waves 16 to 23) were not clean; wave 23 had 5 P0 and 9 P1 |
| 3 | Every open P0/P1 ledger row is closed | met since 2026-09-28; the open rows are P2/P3 |
| 4 | Feature freeze for the window: no new language surface or std API unless it is a sweep finding | not declared; new surface still lands |

Rules for counting:

- A **sweep** is a hunt wave the size of W11-W14. It covers disjoint domains and reproduces each
  finding in the repo. It judges each finding against a Go, CPython or Rust program that you run. It
  also runs the TICKET-167 seed sweep.
- Findings in `std.*`, `std.net`, FFI, diagnostic wording, the LSP and perf do not reset the count.
  The JIT does not compile them.
- A core P0/P1 found by a sweep resets the count to zero once it is fixed.
- At JIT start, the bytecode and the `Value` layout freeze. The JIT's correctness oracle is the
  interpreter against the JIT, byte-identical on `tests/chz` and the CPython differential corpus.

## Ledger

[`docs/contributing/gaps.md`](docs/contributing/gaps.md) is a short ledger of open rows. Each row is
re-verified on the release binary. **7 open rows** today: W8-19, W13-28, W15-4, W15-5, W15-10,
W17-1, W23-1. All are P2/P3 or tracking rows. `tests/gaps_ledger_count.rs` checks this count.
After each merge, re-derive it with `grep -c '^| \*\*W' docs/contributing/gaps.md`: a merge can
produce a wrong counter with no conflict marker. Closed rows are in git: `git log -S '<row id>'`.

Standing lessons from past waves:

- The mechanical oracles (panic-fuzz, CPython differential) find almost nothing. Findings come from
  hand-built programs, judged against a Go, Rust or CPython reference that you run.
- Sample a flaky rate (n >= 5 per side) before you make a causal claim.
- Read a closed row's prescription before you implement it again. Some filed fixes (W8-2, W8-7, W9-5)
  were measured wrong.

## Correctness bar

There is one engine. A test compares against a literal golden, never against a second run. Correctness
is judged against the owning ancestor: Go for concurrency, Python for scripting feel and the `Executor`
family, Rust for enums and errors. Run the reference program and cite its output. The standing detectors
of accidental divergence are the CPython differential (`src/difftest/`) and the two-worker-count run of
`tests/chz` (`tests/chezzi_threads_cli.rs`).

## Perf track (M19, paused)

M19 is paused beside the refactor. Each perf change must preserve behaviour: a VM speedup that changes
output, or that holds at one worker count only, is a bug.

- **Landed:** peephole and constant folding, superinstructions, the `invoke_value` clone kill,
  in-place call args, stringify into a buffer, global slots, `ConstStr` interning, the struct-field
  inline cache, FxHash, call-loop flattening, small-string optimization, the 8-byte pointer-tagged
  `Value`, the method-call inline cache, hot opcodes inlined in `run_until`, binop quickening and the
  4-way adaptive `CallMethod` cache.
- **Deferred:** call-site specialization for `Op::Call` (no gain after the inlining).
- **Gap to CPython** (sweep of 2026-08-14): 1.06× (`loop`) to 4.00× (`poly_method`) slower; `fib`
  2.84×. Startup is about 3.9× faster. Current figures are in
  [`docs/contributing/benchmarks.md`](docs/contributing/benchmarks.md).
- **Shared heap, open:** `map_str` at T=1 is 1.10-1.13 against the pre-merge base `772e0bb8` (owner
  accepted 2026-10-29); `CHEZZI_GC_STRESS=1` timeouts in `tests/chz` that also occur on the base;
  the longest stop-the-world pause is bounded at 100 ms until concurrent mark
  (`heap_stats_report_stw_pause`, design: [`docs/design/go-shared-heap.md`](docs/design/go-shared-heap.md)).
- **Next:** the JIT (Cranelift method-JIT), after the JIT entry rule holds. String concat and `split`
  builder remain. Register VM and generational GC have low return: GC is at most about 11% of runtime.

To do a perf task:

1. Measure first: `cargo run --release -- run benches/run.chz`.
2. Land the change behind a correctness test that fails first and then passes.
3. Keep the suite green at both worker counts.
4. Measure again. Record the delta in `docs/contributing/benchmarks.md`.

Do not trust the expected payoff of a lever before you measure it. Several levers moved a different
bench than predicted. Ranked levers are in [`docs/design/roadmap.md`](docs/design/roadmap.md) §4.

## Later

- **M25, killable subprocesses** (design written 2026-08-12, not implemented). Cancellation is
  cooperative, so a task inside `process.cmd("sleep 5")` ignores a sibling fault, `os.exit` and
  `chezzi test --timeout=500` for about 5 s. The change applies to `std.process` only. Design:
  [`docs/design/killable-subprocess.md`](docs/design/killable-subprocess.md).
- **Bug discovery:** a cargo-fuzz parser target, Miri and sanitizers, proptest, metamorphic tests.
  Plan: [`docs/contributing/bug-discovery.md`](docs/contributing/bug-discovery.md).
- **FFI:** the remaining surface is `str` struct fields, stored or cross-thread callbacks,
  pointer-deref builtins and a Rust userdata handle.
- Design and ergonomics backlog: [`docs/contributing/backlog.md`](docs/contributing/backlog.md).

## Key links

| what | where |
|---|---|
| index of all docs | [`docs/README.md`](docs/README.md) |
| open-row ledger | [`docs/contributing/gaps.md`](docs/contributing/gaps.md) |
| owner of each fact | [`docs/contributing/owners.md`](docs/contributing/owners.md) |
| engineering lessons | [`docs/contributing/lessons.md`](docs/contributing/lessons.md) |
| roadmap and design proposals | [`docs/design/roadmap.md`](docs/design/roadmap.md) |
| this month's dated log | [`docs/history/progress/2026-11.md`](docs/history/progress/2026-11.md) |
