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

Concurrency is complete through Tier-D: `spawn`, the `parallel:` nursery, `Channel[T]`,
`Broadcast[T]` (`std.concurrency.broadcast`), `Shared[T]`, `Executor`, the OS-thread M:N engine,
the netpoller and `std.net`. There is one engine: the bytecode VM on its M:N scheduler. On the
branch `exp/go-shared-heap`, tasks share one heap and get values by handle (Go's model; see the
experiment section below).

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
| 3, fact 11 | one `WireValue` child visitor: `WireValue::visit` owns what a variant holds (TICKET-267) | done 2026-10-19; deleted with the wire form by M3.5 (branch `exp/go-shared-heap`) |
| 3, airlock capture copy | one owner per capture, `WireMemo::sole_captures`: every rebuild of a closure marks its captures, a same-task round trip and a relay included; only a box call's result and an Executor job's result stay unmarked, for a capture that reaches no copy and nothing writable the source keeps (TICKET-295) (TICKET-285, TICKET-291, concurrency-facts Family 1) | done 2026-10-22; deleted with the airlock by M3.5 (branch `exp/go-shared-heap`) |
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
     `import b.math`) binds `math` once, so a third binding clashes. TICKET-283 closed the
     low item: a nested fn read only by itself warns `function 'f' is never used` (rustc `dead_code`).
   - TICKET-266, a `never` return type (filed 2026-10-18): `exit` and `panic` declare `-> never`,
     and a call of that type leaves its block (Rust `-> !`). It deletes the `panic("unreachable")`
     lines that TICKET-263 put after `os.exit`.
   - TICKET-265, group C, builtin types and protocols. Part 1 is done (2026-10-17): the std
     protocols live in `std.ops`/`std.cmp`/`std.iter`/`std.string`/`std.path`/`std.convert` and a bound
     needs the import, `Hashable` embeds `Eq`, `T?` derives no ordering. A std import checks and
     compiles only the std fns the program reaches, and a process decodes each embedded std module once from a build-time AST (`shared_std_parse`, item 6; TICKET-265 had it lex and parse the text once); `std.iter`/`std.string`/`std.ops` imports still
     cost 2-3 ms over `print(1)` (`docs/contributing/benchmarks.md`). Part 2 landed (2026-10-21): the method set per
     builtin type is generated from the std native decls and the collection convention rows are in
     (TICKET-278); `Hashable` needs `hash` and `eq` for a user type (TICKET-280). Part 2 also had what an
     Iterable yields and the Deque, Heap and Counter iteration: landed (TICKET-279). Still open: the
     U5 sibling gaps.
2. **Two type resolvers** (found in TICKET-264 review, 2026-10-17): one arm table decides what an
   annotation names; the read-only, mutable and C-type walks, the annotation hover and the return-only
   extern check read it; the grids run every arm; done 2026-10-21 (TICKET-281).
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
6. **Std modules compiled at build time** (owner decision 2026-10-18, option 1; TICKET-282, filed 2026-10-21).
   Lex and parse: done 2026-10-21. `crates/chezzi-native/build.rs` parses each `std/**/*.chz` with
   `parser::parse_module_source` and embeds the `borsh` AST, the identifier words and the `protocol`
   statements; a run decodes them (`std_embed::decode_ast`) and lexes no std text. `std/*.chz` stays the
   one source; the next `cargo build` picks up an edit. Release, `hyperfine -N -w 5 -r 40`, base and
   branch in their own target dirs: `print(1)` 8.0 to 6.5 ms, `import Iterator from std.iter` 10.6 to
   9.0 ms. Open: check and compile caching. A std import still costs about 2.5 ms over `print(1)`
   (graph build, check, compile), and the reach set (`resolver::unreached_std_fns`) and the
   program-wide slot and type keys depend on the importing program. CPython pays ~0 ms for
   `itertools` (C code in the binary) and loads saved bytecode for a pure-Python module (`.pyc`).
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
9. **One `Executor.submit` (TICKET-289, done 2026-10-22).** `ex.submit(f) -> Task[T]` is the only
   submit form; `submit_result` and the bare submit are gone. `ex.cancel()` replaces `shutdown_now()`
   and returns at once; `t.cancel()` is new. A job fault or cancel appears only in `t.get()`:
   `shutdown()`, the run end and a test end raise no ordinary job fault, and a fault cancels no
   sibling. A `--max-heap` abort is never a Task outcome and a `--timeout` cut leaves the Task
   unsettled. A refused submit returns an already-settled Task. Open row: a job parked in a stuck
   `defer` after a nursery child fault, with a cut sleeping body, is not reported as a deadlock (the
   base binary has the same miss).
10. **Bug-hunt sweeps** resume after the refactor. They count toward the JIT entry rule below.

### Open questions for the owner

None open on 2026-10-16.

### Owner decisions not yet implemented

- **U16: `Sized` and `Extend` protocols not declared yet** (owner decision 2026-10-16).
  A protocol only where generic code needs one; a new protocol goes in a std module, not the global
  namespace. The convention rows themselves are done (TICKET-278, 2026-10-21): Set
  `insert`/`remove` return `bool`, Map `insert`/`remove` return `V?`, List `remove(i)` returns `T`,
  and the concurrent collections follow the same rows.

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

**D4 (approved 2026-09-22, superseded 2026-10-29 on `exp/go-shared-heap`).** On `main`, a spawned
task's write to an airlock copy is a check error or a runtime fault. On the branch, no route copies:
a task's write lands on the parent's value and a race is the user's bug (Go). Record:
[`docs/design/adr-d4-airlock.md`](docs/design/adr-d4-airlock.md); current rules:
[`docs/reference/concurrency/01-model.md`](docs/reference/concurrency/01-model.md).

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

## Experiment: Go shared heap (branch `exp/go-shared-heap`)

- M1.7: one heap per run, collected with the world stopped from every root source (attached threads, detached threads, parked fibers, box contexts); crossings still copy; `--max-heap` caps the whole run. Design: [`docs/design/go-shared-heap.md`](docs/design/go-shared-heap.md).
- M1.8: a value held in a Rust local across a blocking door survives another thread's sweep: a `recv` wakes senders through the channel core, a `Shared` method roots its receiver; an extern C call with a callback runs detached and its callback reattaches (Go `cgocallback`); printing a busy `Writer`/`Reader` waits detached. Gate: `tests/gc_blocking_pins.rs`.
- M1.9: M1 exit gate. `tests/chz` 1853/1853 at T=1, 2, default; stressed `spec` (GC_STRESS, T=2): 140 pass, 0 dangling, 4 timeouts; max stop on `many_list` under 10 ms (`heap_stats_report_stw_pause`), but the counter then timed only the wait for the world to stop (M2.8 corrects it). M2 is next.
- M2.2: `List`, a cursor's `Iter.items` and `bytearray` live on an `RVec` (`vm/rvec.rs`; bytes
  pack one per byte): a grow, sort or reverse retires the old block, and only the stop-the-world
  sweep frees it. Gates: `list_growth_retires_until_collect`, `bytearray_growth_retires_until_collect`.
- M2.3: `Map`/`Set` entries live on an `RVec`, and their hash index is an open-addressing table
  on an `RVec` of atomic `(hash, position + 1)` slots, linear probing, at most half full
  (`vm/map_data.rs`). A grow past half full or a remove builds a new table; each old entry block
  or table retires until the stop-the-world sweep. A racy probe reads only live blocks, at most
  `capacity` slots, and only positions below the entry count; it can miss or see a torn entry
  (user fault, Go parity). Its live bytes count the index. `Obj` is now 40 B.
  Gates: `map_insert_growth_retires_index_and_entries`, `set_insert_growth_retires_index_and_entries`,
  `constant_hash_growth_retires_old_index`, `racing_insert_and_probe`.
- M2.4: a struct's spilled fields, a closure's captures, an enum payload and a module's global slots live in fixed `RVec` blocks (a write is one atomic slot store; an empty payload allocates nothing; a new global's append retires the outgrown block). A parked generator sits behind one atomic owner pointer (`rvec::AtomicBox`): a resume from a second thread faults "generator already running" instead of tearing its frames; reads of a parked generator (GC trace, a crossing copy) run at once and never fault each other, and a Rust panic inside a resume leaves the generator done. `Obj` stays 40 B. Gates: `spill_field_write_is_slot_store`, `generator_ctx_take_is_exclusive`, `generator_next_from_two_tasks_faults_cleanly`, `tests/chz/spec/generator_shared_read_test.chz`.
- M2.5: two threads that race on one list and one map (10,000 ops each, plain and `CHEZZI_GC_STRESS=1`) give racy values or faults and never read freed memory (Go's rule, [`01-model.md`](docs/reference/concurrency/01-model.md) §Data races). `Value` is one 8-byte aligned word (a const assert). Gate: `two_vms_race_on_one_list_and_map_without_uaf`; the test helper `share_handle` keeps the root's roots published while workers run.
- M2.6 fix 1: a map or set probe restarts when a user `eq` pushes or removes (a per-table version count), so an `eq` that removes another key no longer hides a present key (`tests/chz/spec/probe_eq_mutation_test.chz`); `AtomicBox<T>` is `Sync` only for `T: Send + Sync`; 01-model.md lists the four shared-heap holes open before M3.
- M2.7: an `RVec` length is 64-bit (the block header is a `u64` cap and an `AtomicU64` len, 16 B), as a Go slice's `int`: `bytearray(4294967296)` works again, in the entry task and in a spawned task. One allocation path (`try_alloc_block`) returns `AllocFailed` past `isize::MAX` bytes or on a null allocation; `bytearray(n)` and `[x] * n` turn it into their clean fault, every other grow panics as a `Vec` does. `bytearray(n)` no longer writes its zeros (the allocator zeroes the block). Gate: `tests/chz/spec/alloc_fault_grid_test.chz`.
- M2.8: profiled the M2 gate against main and fixed six causes: the sweep moved each dead `Obj` to the stack and cleared its copy bit with a locked `fetch_and`; `callable` was no longer inlined; a write read the object's size twice; closure captures, a `for` copy and an `RVec` clone went through a temporary `Vec`; a debug build ran layout math per slot. Single-thread new/main (median, both orders): closure 1.15 to 1.04-1.08, hof 1.16 to 1.03-1.05, str 1.18 to 1.06, unique 1.20 to 1.05, list 1.08 to 1.06. Still over 1.05: str (1.06-1.07), list (1.06), map_str, many_list, many_map (1.07-1.17). Peak RSS: list 1.57 to 1.01; map 1.58 and storm T=4 1.32 still over. Sched: storm T=2 1.25, spawn_fresh_graph T=1 1.10-1.16. `stw_max_us` now times the whole stop: `many_list` stops for 44 ms (release), so `heap_stats_report_stw_pause` fails until concurrent mark (design stage 5); `executor_task_grid`'s native-hof cell is 3.0 s in debug (base 4.4 s, bound 2 s).
- M2.9: a map or set index slot is one 8-byte word (a 24-bit hash tag and a 40-bit `position + 1`; a tag hit is checked against the entry's cached hash; at most `2^40 - 1` entries, one more is the `RVec` capacity fault), so one store publishes a slot and the index is half its M2.3 size: `map` peak RSS 35.5 to 27.6 MB. Gate: `tests/chz/spec/map_index_grid_test.chz`. Retired `RVec` bytes start a collection when they reach the live bytes of the last sweep (at least 1 MiB; each retire list counts its bytes): a 100k-key map grow loop now collects during the run (base: no collection, `map_grow_loop_collects_on_retired_bytes`); `map` peak RSS 27.6 to 24.6-25.6 MB (main 22.6-22.8, ratio 1.08-1.13). The dev and test profiles build `chezzi-vm` at opt-level 1: `cargo test -p chezzi-vm` 62 s to 35 s, the native-hof cell of `executor_task_grid` 2.7 s to 0.40-0.46 s (bound 2 s, 3/3 pass). `heap_stats_report_stw_pause` bounds the longest stop at 100 ms until concurrent mark (owner ruling; was 10 ms): the test build's longest stop on `many_list` is 69-80 ms, release 46 ms. Still over 1.05 against main: `map_str` 1.09-1.18 (base 1.01).
- M3.0a: a detached thread reads no heap: the door body gets a `Detached` handle, not the `Vm`; a native or C call in the door reads its args copied out before the door (`Vm::copy_arg`, the one owner of the typed arg reads), and a C callback re-attaches. Gate: `a_detached_native_reads_a_shared_list_without_uaf` (base: `dangling GcRef`). The print walk roots the element copies it holds across a door or a `str` hook (`the_print_walk_roots_what_it_holds_across_a_door`, base: `dangling GcRef`); a close drops its backing after it releases the handle's lock, so a render that cannot wait reads `Writer(closed)` during a close (`a_render_during_a_close_reads_closed`; was `open`). A racy container op never panics: each reads one snapshot of the buffer (`RVec::view`, `try_get`, `remove` gives `None`, `insert` clamps; a map or set entry another thread removed is a miss) and gives its own out-of-range fault, which `recover:` catches (`racy_container_ops_never_panic`, a 41-op grid; base: 17 ops panicked).
- M3.0b: a `Value` slot (an `RVec` slot, a map entry word, a struct field, a cell) stores with Release and loads with Acquire (`rvec::VALUE_STORE`), so a thread that reads a `GcRef` sees the object it names (Go's `publicationBarrier`). No op holds a `&mut Obj` over a reachable object: a write takes `Heap::write` and a `&self` mutator; a struct's inline fields, a cell, a cursor position and a generator stamp are `rvec::RCell` slots, a module's names sit behind a `RwLock`; `Heap::get_mut` is now `init_mut`, for an object the call built and has not published. Gate: `shared_object_mutation_grid` (base: `free(): invalid pointer`, SIGABRT). Release, single thread, new/base median: struct 0.99, poly_method 1.00, closure 1.00, hof 1.01, list 1.01, many_list 0.98, map 0.93-0.98, unique 1.01-1.02 (30 runs, both orders); `storm` at T=2 0.99-1.01.
- M3.1: module globals are shared (Go): a task, an Executor job and a box closure read and write the run's one copy of each global (`Vm::prepare_worker` hands the worker the spawner's module objects; a box call takes its caller's). Deleted: the per-task module snapshot and its lazy fault/replay, the TICKET-213 view stamps (`vm/heap_view.rs`), the snapshot cell/node registries and adoption, `Route::ModuleSnapshot`, the global-write D4 fault and `BOX_COPY_WRITE_NOTE`; the checker's write gate stops at scope 0 (`is_local_capture`). One fn-value table per run (`Vm::fn_value`), so a fn read from a global and one received by copy stay one value. Gate: `tests/chz/spec/shared_globals_test.chz` (11 rows, Go goldens; base: `'N' is this task's copy`). A parent-made alias of a global that a task captures is still a crossing copy until M3.4. Fix 1: one module table per run (`vm/module_table.rs`), so an Executor job submitted by an earlier module reads a later module's global (Go `job got 49`; base: `RVec index out of range` panic). Release, new/base median (5 runs): fib 0.88, loop 1.00, closure 1.00, hof 1.04, struct 0.88; T=2 `storm` 0.011 (3403 ms to 38 ms), `spawn_fresh_graph` 0.77.
- M3.1b: a value a `?` or a `return` carries out of a `parallel:` or a `recover:` rides the VM stack across every drain before its delivery (`do_try`, `do_return`, `run_invoke`); `do_try` held it in a Rust local, and another thread's collection freed it (since 291c80df). Gate: `tests/try_drain_rooting.rs` runs `tests/chz/spec/try_drain_rooting_test.chz` (7 rows) under `CHEZZI_GC_STRESS=1` at T=1 and T=2 (base: 4 rows fail 10/10, `dangling GcRef` or `cannot match on List`). The release-only wall-clock tests in tests/chz read the build mode from `CHEZZI_TEST_DEBUG_BINARY`, which the Rust runners set from their `cfg!(debug_assertions)` (`chz_dirs::set_build_mode`), not from a timed burn. The CPU-measuring `threads_*` tests scale their burns to the cost they were measured at (`child_rusage::burn_scale`).
- M3.2: a channel (buffer, a parked sender's offer, a parked receiver's slot), an `Atomic` and a sealed Task channel hold a `Value`, so a send, a store and a seal copy one word (Go `chan T`, `atomic.Value`); the collector reads a core's values under its lock (`Heap::children`), and a party blocked in place or demoted pins the channel handle in its GC door. A Task outcome a job transition seals with no heap is text, built into its `Err` value by the first receive (`Vm::recv_ready`). The print path snapshots a nested `Atomic` before its next door. Deleted: the nested-core walks and summaries (`SeenCores`, `collect_core_gcrefs`, `wire_summary`, `WireSummary`, `*_bytes_deep`, `mark_core_payload`), the Executor queue, `WorkerResult.value`, `JobSettle.cancel`, `WireMemo::job_result` and the checker's Channel element sendable rule. A Task's error now keeps the job's origin (CPython: a future's exception keeps its traceback). Gate: `tests/chz/spec/core_handle_test.chz` (11 rows, Go goldens; base: 10 rows `1 == 2`) and `a_value_held_only_by_a_core_survives_a_stop` (6 holders, `CHEZZI_GC_STRESS`, 20/20 at T=1 and T=2; base: every row `1`). A spawn capture is still a copy until M3.4 (`grid_policy_r49`). Release, new/base median (5 runs): fib 0.96, closure 1.01, list 1.00; T=1/T=2 `send_one_channel` 1.00/0.92, `rendezvous_pingpong` 0.95/0.53, `storm` 0.98/1.02, `executor_and_parallel` 0.96/0.98.
- M3.3: `Shared` / `RwShared` hold ONE value by handle in an atomic slot (`SharedCore.v`), behind one guard (fix round 1: one atomic lock word per box owns the uncontended path, a reader count, a writer bit and a waiting-writer count; the process-wide wait-for graph is read only on a contended acquire; readers share, a waiting writer keeps new readers out from its first failed acquire to its take, a nested read past a waiting writer faults as a lock cycle; fix round 2: a write release admits the readers that waited during the write, as Go `RWMutex.Unlock` does). Every method and closure runs in the caller's task on the live value (Go `mu.Lock(); f(v)`): `get()` returns the live value (write it only inside `update`), a closure writes its captures, a blocking closure keeps the guard with its task, a `print` takes no guard. A `write` inside the same task's `read` faults `already holds the read guard on this RwShared box` (Q2). Deleted: `vm/boxctx.rs` (box contexts, `with_box`, `box_chain`, the copy-in/out walkers), `WireMemo::box_result`/`sole_captures`, `Route::CopyRead`, `Kind::InterceptAirlock`, `std.concurrency.is_task_copy`/`task_copy_of`, `Task._cached`, the memoize alias map, the TICKET-089 box-read warning. Collection walks copy shallow under the read guard. Gate: `tests/chz/spec/shared_handle_test.chz` (13 rows, Go goldens; base: 5 FAIL, 3 ERROR), `m3_3_guard_modes_grid`, `a_guard_and_channel_cycle_gets_the_deadlock_verdict`. Release, new/base median (5 runs): fib 1.06 (reverse 0.97, 10 runs 1.02), closure 0.99, map 1.06 (reverse 0.90, 10 runs 0.96); `update_read` 0.44/0.46 (T=1/T=2), `shared_read` 0.82/1.00, `cross_map` 0.03/0.02, `lookup_100k` 1.09 (reverse 0.89)/0.96, rwshared stress 0.55 (T=2). Fix round 1 (vs BASE): `RwShared.get` x 4M 0.80 (T=1 and T=4), `Shared.get` 0.78, 4 parallel readers 0.93, read mix 0.55/0.48, fib 1.01, closure 1.00, map 0.98.
- M3.4: `spawn f(args)`, `spawn obj.m()`, `spawn:` captures and `Executor.submit(f)` hand the task the parent's values by handle (Go `go f(x)`): a task's write lands on the parent's object and a captured local is one binding with the parent (`Vm::prepare_worker` builds the call from the spawner's values). A shared generator that a second task drives while the first runs it faults `generator already running` (CPython `ValueError`). Deleted: `deep_clone_all`, `lower_task`, `rebuild_ready`, `wire_args`, `ensure_crossable`, `Lowered`, the spawn op's fresh `u32` and `Program.fresh_calls`; the checker's layer A (`note_task_write`, `report_named_call_writes`, `is_local_capture`, the W8-3 taint and closure-write frames, the spawn and capture `sendable` gates, `CROSSING_CLOSURE_METHODS`), the fn write summaries and `self_writers` (`checker/self_writes.rs`); `vm/airlock_in.rs` is test-only until M3.5. Gate: `tests/chz/spec/spawn_handle_test.chz` (6 rows, Go goldens; base: 6 type errors, an Executor write lost, a fault). Release, new/BASE median (5 runs): fib 1.03, closure 1.00, hof 0.99, struct 1.01; T=1/T=2 `storm` 1.00/0.87, `spawn_fresh_graph` 0.56/0.46, `executor_and_parallel` 1.01/1.00; native-hof 0.79.
- M3.5: the airlock is gone: deleted `vm/wire.rs`, `vm/airlock_in.rs`, `vm/airlock_out.rs` (`start_task` moved to `vm/nursery.rs`), `base/crossing.rs`, the copy bits and copy logs of `HeapCore`/`Heap`, the five layer C fault sites and `COPY_WRITE_TAIL`, `Op::StampGen`, `Proto.private_slots`/`deep_slots`, the generator frame verdicts and param-escape summaries, and the checker's `sendable` (its two last callers had no reachable reject: 14 probes, same output on BASE). `Fresh` (TICKET-254) lives in `checker/fresh.rs`. A box renders the live value (`Vm::display_box`): `print(Shared(xs))` with `xs.push(xs)` is `Shared([[...]])` (was `Shared([<cycle>])`), one list twice is `Shared([[1], [1]])`. Gate: `airlock_state_is_gone_from_proto_and_heap_core` (Proto 152 to 136 B, HeapCore 512 to 464 B). Release, new/main median (`-w 2 -r 7`): fib 0.99, struct 0.88, closure 1.02, hof 1.00, many_list 1.08, map_str 1.10-1.13 (f900b091 1.12 and BASE 1.13 too: not this task); `storm` T=2 36 ms (f900b091 3361 ms), `spawn_fresh_graph` 0.41-0.52x of f900b091; `send_one_channel` T=1/T=4 0.91/0.76 of f900b091, T=2 even (high variance). Peak RSS storm T=4 11.8 MB (f900b091 104 MB), many_map 94.7 MB (main 169.5).
- M3.6: no test or doc example asserts a race's result (Go parity: a race is the user's bug). Audit of `tests/chz`, `examples`, Rust tests and `docs/reference`: 4 racy rows, all the `xs`/`fill` shape or a loop of tasks that write one map; the rest is ordered by a join, a Channel or a box. `spawn_handle_test.chz` orders the two writes with a `Channel` and its loop tasks send their pairs to the parent; `01-model.md` and `07-sendability.md` show the ordered form. Gate: 3 x 5000 rounds at `CHEZZI_THREADS=4` fail 0 (HEAD: 1 and 54 of 5000); the Go translation runs clean under `go run -race`.
- M3 is closed (M3.0a to M3.6): tasks share values by handle, and the airlock, the wire form and the D4 rule are deleted.
- M4: the docs state the shared-heap model at each owner (`concurrency/01-model.md` the model and the race rules, `03-shared.md` the boxes, `07-sendability.md` retitled "What crosses a task boundary", `stdlib/03-runtime-types.md` the Executor job); ADR D4 carries a superseded note; `go-shared-heap.md` gets its status and an outcome section (§7); `lessons.md` §4 drops the copy-model rules (`lessons-incidents.md` notes each change). Gate: the audit grep has 0 stale rows (74 before); `doc_fences_lex`, `doc_links`, `conformance`, `source_layout`, `gaps_ledger_count` pass.
- M4b: the `tests/chz/spec/airlock_*` files and `examples/airlock_cycle.chz` are renamed `crossing_*` for what they check. The `owner_fault_grid` `Nursery#16` flake was a runtime bug: a child's fault tripped its scope's cancel flag before `finish` recorded the fault, so an owner parked at an inner join read a tripped flag with an empty slot (`block::halt_of`), took no halt and ran the line after its join. `MnSched::finish_with` now trips the flag under the core lock with the slot (the one trip of a task's aborting end; `Vm::trip_cancel` is deleted). Gate: `an_owner_at_an_inner_join_is_cut_when_its_childs_fault_is_not_yet_recorded` (5 owners x 4 contexts, base: 15 of 20 cells print `after`), `an_aborting_outcome_trips_the_fibers_cancel_at_finish`; under 28 CPU burners, base 19/6000 runs print `after`, new 0/12000.
- M4c: the Rust test fns and files named `airlock_*` are renamed `crossing_*` (64 Rust fns, 2 Chezzi test fns, 4 files; `airlock_state_is_gone_from_proto_and_heap_core` keeps its name: it checks the deleted state). The closure-route asserts that looked for the deleted "is this task's copy" text (`crossing_closure_readout_test.chz`, `crossing_one_binding_test.chz`, `box_grid_test.chz`) now fail on any fault and check that the write lands. `main` (772e0bb8) has the same trip-before-finish bug: under 28 CPU burners its release binary prints `after` in 8/6000 runs of the six cell programs and 13/2000 of `fiber_nested_plain`.
- M5: a sweep's graveyard (the freed objects whose drop makes a syscall) drops through the GC door (`Vm::drop_graveyard`); it dropped attached since 291c80df, so a garbage writer whose tail flush blocked on a full pipe held every later stop, and a run whose reader was its own task hung. A dropped file-backed writer still flushes its tail (Python and Rust parity), on the task that collected. Gate: `tests/gc_graveyard_drop_grid.rs` (3 writer kinds x T=2/default, base: 6 of 6 cells hang). `Atomic.cas` on an iterator or a generator payload compares by its object, as `==` does (was a fault with a copy-model reason; Go compares a pointer by address); a fn payload stays refused (Go's func is uncomparable). Gate: `atomic_cas_compares_an_iterator_or_a_generator_by_identity` (base: the cas fault). Deleted the dead `Heap::request_collect`/`HeapCore::request_collect` (only a unit test called them). Test, comment and doc rows: the cancel-trip tests use `finish_with` (the code's order), the `--max-heap` sleeper rows in `05-cancel.md` state fresh measures, and the example and test comments state the shared-heap model.
- Open rows on the branch: `map_str` at T=1 is 1.10-1.13 against `main` (owner accepted 2026-10-29; f900b091 1.12 and the M3.5 base 1.13 too); `CHEZZI_GC_STRESS=1` timeouts in `tests/chz` that also occur on the base.

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
