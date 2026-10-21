# Chezzi progress tracker

Chezzi is a fast, statically typed scripting language with a Python feel, built by hand in Rust.

This file is the single source of truth for "what is next". It holds the current state only.
The dated log of finished work is in [`docs/history/progress/`](docs/history/progress/).

## How to update this file

- Write each new dated entry at the top of `docs/history/progress/YYYY-MM.md` for the current month.
  Create the month file when the month starts. Newest entries go first.
- Change this file only when the phase, the focus, the next steps or the ledger count change.
- Keep this file at 300 lines or fewer. Move a finished section to `docs/history/progress/`.
- Re-derive the open-row count after every merge (see [Ledger](#ledger)).

## Where things stand

The core language is implemented through M24 and still evolves:

- scalars, `List`/`Map`/`Set`/`tuple`, generic structs and enums;
- `T!E` / `T?` with `?`, `!e`, `?x` and implicit wrap; `None` is a keyword;
- generics with structural protocols, user `==` through `Eq`, static protocol requirements through
  witness passing;
- exhaustive `match` with guards, closures and higher-order functions;
- modules, including Python full paths (`a.b.X`);
- GC, interpolation, pipe, `defer`, `recover:`, `Iterator[T]`, slicing and indexing protocols, ranges
  as values.

Concurrency is complete through Tier-D: `spawn`, the `parallel:` nursery, `Channel[T]`, `Shared[T]`,
`Executor`, the OS-thread M:N engine, the netpoller and `std.net`. There is one engine: the bytecode VM
on its M:N scheduler.

The checker has a non-fatal warning channel (`Severity::Warning`, `"severity"` in `--errors=json`,
`DiagnosticSeverity::WARNING` in the LSP). It has five rules:

1. a discarded `T!E` / `T?`;
2. a mutating call or a store on a temporary copy (a `Shared`/`RwShared`/`Atomic` read, a builtin
   slice, `.copy()`, an operator result);
3. an unreachable `match` arm;
4. an unread local;
5. a near-miss hook that its operator skips.

Test totals change with each ticket. Read the live counts from `cargo test` and
`chezzi test tests/chz`, not from prose.

## Current phase: root-cause refactor before the JIT

Bug-hunt waves 16 to 23 (JIT sweeps #1 to #8, 2026-09-28 to 2026-10-11) were not clean. The root-cause
records trace their findings to systemic families: Names, Blocking, Declarations, Airlock and
Identity. In each family, one fact had several deciders that were kept in sync by hand. An audit of
TICKET-120..176 found that 34 of 51 fixes were point patches, and each family patched that way came back.

**Rule: derive, don't mirror.** Each fact has one owner, and other code reads it. A fix deletes a
duplicate or adds a single source. See [`docs/contributing/lessons.md`](docs/contributing/lessons.md) §1.
Before you change core code, read the root-cause record of the area:
[w16](docs/history/bug-hunts/w16.md) to [w23](docs/history/bug-hunts/w23.md).

The refactor runs in phases. The owner index is
[`docs/contributing/owners.md`](docs/contributing/owners.md): it names the one owner of each fact and
lists each remaining copy as DUPLICATE.

| phase | what | status |
|---|---|---|
| 0-1 | owner index and build baseline | done 2026-10-15 |
| 2 | one topic per file (byte-exact moves; `tests/source_layout.rs` guards size and topic lines) | done 2026-10-15 |
| 3 | one owner per fact: facts 1-9 (blank name, places, `self`, type-application head, operator hooks, pattern names, reserved names, std module members, type keys) | done 2026-10-16 |
| 3, fact 8 step 2 | builtin dispatch by an id (`enum Builtin` in `base/builtin.rs`) | done 2026-10-16 |
| 3, names | one redeclaration rule; a module binds each name once, a type and a value never sharing it (TICKET-263) | done 2026-10-18 |
| 3, fact 11 | one `WireValue` child visitor: `WireValue::visit` owns what a variant holds (TICKET-267) | done 2026-10-19 |
| 5 | workspace crates: `chezzi-syntax`, `chezzi-native`, `chezzi-base`, `chezzi-front`, `chezzi-vm` | done 2026-10-16 |
| docs | docs split into `reference/`, `design/`, `contributing/`, `history/` | in progress (branch `docs/refactor`) |

Details of each step are in [`docs/history/progress/2026-10.md`](docs/history/progress/2026-10.md).

## What is next

Each group below is one fact decided in two or more places (grouped 2026-10-16).

1. **In the pipeline (filed 2026-10-16):**
   - TICKET-263, group A, names: merged 2026-10-18 (one owner for which names hold a slot; one name
     per module; a let binder is not readable in its own value; `os.exit` is an ordinary call, so
     write `panic(...)` after it). The owner hand-merged it after `verifying` escalated on one
     `else_guard_grid` cell, with one review fix: an ambiguous import pair (`import a.math` +
     `import b.math`) binds `math` once, so a third binding clashes. Open, low: a nested fn read only
     by itself warns `unused variable`, where rustc says `function is never used` (TICKET-283).
   - TICKET-266, a `never` return type (filed 2026-10-18): `exit` and `panic` declare `-> never`,
     and a call of that type leaves its block (Rust `-> !`). It deletes the `panic("unreachable")`
     lines that TICKET-263 put after `os.exit`.
   - TICKET-265, group C, builtin types and protocols. Part 1 is done (2026-10-17): the std
     protocols live in `std.ops`/`std.cmp`/`std.iter`/`std.string`/`std.path`/`std.convert` and a bound
     needs the import, `Hashable` embeds `Eq`, `T?` derives no ordering. A std import checks and
     compiles only the std fns the program reaches, and a process parses each embedded std module once (`shared_std_parse`; debug, fill vs clone: `string.chz` 3.65 vs 0.19 ms, `iter.chz` 3.24 vs 0.14, `prelude.chz` 5.21 vs 0.23, `ops.chz` 0.60 vs 0.03); `std.iter`/`std.string`/`std.ops` imports still
     cost 2-3 ms over `print(1)` (`docs/contributing/benchmarks.md`). Part 2 is filed (2026-10-21): the method set per
     builtin type and the collection convention table (TICKET-278); what an Iterable yields and
     `Deque`/`Heap`/`Counter` `Iterable` (TICKET-279); `Hashable` opt-in (TICKET-280). Still open: the
     U5 sibling gaps.
2. **Two type resolvers** (found in TICKET-264 review, 2026-10-17): `resolve_ty_ro_d` and
   `resolve_type` resolve the same annotations; `src/checker/resolve_ctype.rs:89`, `:137`, `:145`, `:160`
   say "mirrors". One resolver must call the other (TICKET-281, filed 2026-10-21).
3. **Group E (TICKET-268, 2026-10-19).** `chezzi check` rejects a constant int fault: an overflow, a
   constant zero divisor and a constant shift amount outside `0..64`, whatever the int left operand is
   (`1 / 0`, `x % 0`, `x <<= 64`). One owner for an int op's fault: `int_binop`, `operand_fault`,
   `int_neg` and `IntFault` in `src/ast/consteval.rs`; a binary op's name comes from `HOOKS`. The VM ops,
   `Atomic`, `sum`, the natives and the checker read it. Done 2026-10-19.
4. **Group F, syntax (filed 2026-10-19).** Row 25 waits on G5. Done: postfix `??` and `?..`
   (S2-3, TICKET-272: the lexer reads a `?` run by its spacing); `?` in `defer:` is a check error, as in `spawn:`, and the generator ban
   drops "(experimental)" (U11, TICKET-274); the 1-tuple type `(int,)` and pattern `a, := t` (S2-7, U9,
   TICKET-273: one comma-list reader, `comma_tail`, and one tuple writer, `tuple_tail`, which also fixed
   `Shared((1))` and a `(false)` match witness; the CPython differential now generates 1-tuples); CRLF
   line breaks (U14, TICKET-271: the lexer and the editor read a CRLF pair as
   one `\n`); the `.map[int](f)` chain (U17, TICKET-269: the
   checker infers each value receiver once); a line break inside a hole already works (U8, TICKET-260).
   Owner decisions (2026-10-19): top-level `return` stays an error, as Python's `SyntaxError: 'return'
   outside function` (a manifest entrypoint `-> T!` already ends the program with an error); an enum
   needs at least one variant, and `struct S:` + `pass` is the empty type (Go `struct{}`); `?` in
   `defer:` is rejected, as Rust's `Drop` and Go's deferred func; `defer` in a generator stays banned.
5. **Owner decision for group A (2026-10-16):** a type name (`struct`, `enum`, `type`) and a value
   name (`:=`, `fn`, import) never share a name in one module, as Go does and as Rust does for a
   callable type (`struct E(i32);` + `fn E() {}` is E0428). This also rejects `struct S` + `fn S`.
6. **Std modules compiled at build time** (owner decision 2026-10-18, option 1; TICKET-282, filed 2026-10-21). A
   std import costs ~3.4 ms per run: `chezzi run` of `print(1)` takes 8.3 ms, and 11.7 ms after
   `import Iterator from std.iter` (release, `hyperfine -N -r 40`). Each run parses, checks and compiles
   the embedded std source again. CPython pays ~0 ms for `itertools` (C code in the binary) and loads
   saved bytecode for a pure-Python module (`.pyc`; frozen modules in 3.11+). Fix: run the front end on
   each `std/*.chz` at `cargo build` time and embed the result in the binary; `std/*.chz` stays the one
   source. Open for its plan: the reach set (`resolver::unreached_std_fns`) and the program-wide slot
   and type keys depend on the importing program.
7. **Slow gate grid (TICKET-275, done 2026-10-20).** `channel_handoff_grid` runs one test per
   (context, sender kind, run) triple plus one per named program, from one `grid_tests!` list. The
   slowest test takes 50.6 s idle (load 1.9-3.8) and 85.1 s at load 22-29; base had one 198 s test.
   Its gates failed on `executor_autodrain_skipped_on_os_exit`, a test TICKET-270 exposed: it expected
   a job submitted before `os.exit` to print nothing; the job now waits on a gate (`a94aac76`), and 275
   was rebased onto it by hand.
8. **Test time budget (TICKET-276, done 2026-10-21).** Seven tests needed 111-407 s of the 300 s
   nextest limit. Each slow grid is now one test per row (`grid_tests!` in
   `tests/support/grid_tests.rs`), the `hook_conformance_grid` cap override is deleted, and no test ran
   longer than 60.310 s with two suites at once (two runs: 55.202 s and 60.310 s). `tests/test_time_budget.rs` fails on a `slow-timeout` line
   without `terminate-after = 5 }`; the rule is in `docs/contributing/dev-commands.md`.
9. **Bug-hunt sweeps** resume after the refactor. They count toward the JIT entry rule below.

### Open questions for the owner

None open on 2026-10-16.

### Owner decisions not yet implemented

- **Collection methods** (U16, 2026-10-16): one convention table, with Rust's shapes: Set
  `insert`/`remove` return `bool`, Map `insert`/`remove` return `V?`, List `remove(i)` returns `T` and
  faults on a bad index; the concurrent collections follow the same rows. A protocol only where
  generic code needs one (`Iterable`, `Contains`, `Sized`, `Extend`); a new protocol goes in a std
  module, not the global namespace.
- **`Hashable` is opt-in for user types** (2026-10-16). No `struct` or `enum` is `Hashable` on its own
  (the zero-field struct loses it); a user type becomes a key by defining `fn hash` and `fn eq`, as
  Rust requires a derive. Tuples of `Hashable` stay `Hashable` (Rust, Python).
- **`Deque`, `Heap` and `Counter` are `Iterable`** (2026-10-16). Deque front to back; Heap in no order
  (Rust `BinaryHeap::iter`, Java `PriorityQueue`); Counter like a Map (Python `Counter` is a `dict`).
- **An enum needs at least one variant** (owner, 2026-10-19; replaces the 2026-10-16 "empty enum is
  legal" decision). `struct S:` + `pass` is the empty type (Go `struct{}`). Open: the parser enforces
  the rule only when it sees `pass` (`src/parser/decl.rs:818`), so `enum Z:` with only a method and no
  variant is accepted and runs (TICKET-277).

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

**D4 (approved 2026-09-22).** A spawned task's write to an airlock copy is a check error or a runtime
fault, never a silent lost write. Read [`docs/design/adr-d4-airlock.md`](docs/design/adr-d4-airlock.md)
before any airlock work, and [`w16.md`](docs/history/bug-hunts/w16.md) Family 4.

## Ledger

[`docs/contributing/gaps.md`](docs/contributing/gaps.md) is a short ledger of open rows. Each row is
re-verified on the release binary. **8 open rows** today: W8-19, W13-28, W15-4, W15-5, W15-10,
W17-1, W22-1, W23-1. All are P2/P3 or tracking rows. `tests/gaps_ledger_count.rs` checks this count.
After each merge, re-derive it with `grep -c '^| \*\*W' docs/contributing/gaps.md`: a merge can
produce a wrong counter with no conflict marker. Closed rows and the bug-hunt session logs are in
[`docs/history/bug-hunts/`](docs/history/bug-hunts/).

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
| dated progress log | [`docs/history/progress/`](docs/history/progress/) |
| bug-hunt records | [`docs/history/bug-hunts/`](docs/history/bug-hunts/) |
| index of history | [`docs/history/README.md`](docs/history/README.md) |
