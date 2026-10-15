# Code conventions

The full text of every convention, moved out of `CLAUDE.md` (which every agent turn reads). `CLAUDE.md` keeps one line per rule. Read the entry for the area you touch before you change it.

- Commits: single-line conventional (`feat:`, `fix:`, `chore:`, `docs:`, `test:`). No body.
- Each compiler phase is its own module under `src/`: `lexer` → `parser` → `ast` →
  `desugar` → `checker` → `compiler` → `vm` (the engine of record; its `impl Vm` is split across
  `vm/{exec,arith,call,sched,netio,stmt}.rs`), plus `gc`, `native` (builtins / std),
  `resolver` (module paths). (The tree-walk `interp` engine has been removed.)
- Keep modules small and single-purpose.
- **No wildcard arm on an enum in library or binary code.** `Cargo.toml` denies
  `clippy::wildcard_enum_match_arm`; add an exhaustive classifier on the enum first, name the variants
  second, and raise `PINNED_ALLOWS` in `tests/wildcard_enum_lint_gate.rs` for an `#[allow]` (with a
  one-line reason at the site). Test code is out of scope. Do not turn the `match` into `if let` or
  `matches!` on the raw enum to silence the lint. See `docs/lessons.md` §4.
- **New builtin types/ctors/fns go in their owning `std.*` module (import-gated), NOT the global
  reserved namespace.** The global surface stays minimal: scalars, `tuple`, `Range`, `Channel`,
  `T!E`/`T?`/`Iterator`, structural protocols. (`timer(ms)` moved to `import std.time`; `Shared`/
  `RwShared`/`Atomic`/`Executor` to `import std.concurrency` — all stay reserved names.) Register in the
  module's `.chz` (`native type` / `native struct`; no Rust-side module sig exists) and gate the bare name behind `import` via the per-module licensing set
  (mirror FFI `imported_ffi_types` / `imported_concurrency` / `imported_time`); keep runtime ctor/opcode
  dispatch unchanged (the gate is checker-only name resolution). A pure type/ctor with no runtime module-member value also needs the `bind_import` skip in
  the vm, or `import X from M` faults at runtime — cover it with a test that RUNS the program.
  A global reserved name is a one-way ratchet: moving it out later breaks every example + grammar.bnf.
- **A new native fn's `MEMBERS` entry is `(name, fn, Kind)` — the third element is not paperwork.**
  `native::Kind` says how the engine RUNS it: `Inline` (pure CPU, or it touches host stdio/os state),
  `Blocking` (an off-heap-safe syscall — primitive args in, primitive `NativeRet` out, no heap/stdio
  touch during the call — so the M:N engine offloads it to the dirty pool instead of pinning a core
  worker), `TimedWait` (a deadline WE own — `std.time.sleep_ms` only), `HostWait` (a host wait we do
  not own — the `std.io` stdin readers run inline, and the engine demotes the worker for the call),
  or `InterceptIo`/`InterceptNet` (the engine runs it; the registered fn never executes). Getting it wrong is a live starvation bug, not
  a style nit; omitting it is a compile error, which is the point (`docs/future.md` §3c).
- After merging an auto-task branch (post-gate ships): delete the branch + prune its worktree
  (`git worktree remove --force <wt>; git worktree prune; git branch -D <branch>`). Stale worktree
  `target/` dirs (~1.6G each) accumulate and fill the disk. Delete rejected branches too.
- **Two auto-task runs in parallel MUST NOT share a release target dir.** Both writing the same warm
  `CARGO_TARGET_DIR` (e.g. `~/.cache/chezzi-target`) means one run's `release/chezzi` overwrites the
  other's, cargo then reports "up to date", and the binary you verify SILENTLY LACKS your change — a
  a green test run proving nothing (hit 2026-07-26). Give each concurrent run its own
  `CARGO_TARGET_DIR`, or confirm the binary really contains the change (`strings … | grep <new msg>`).
  Same family as the worktree stale-binary trap: when verifying, also beware that a `cd` into a worktree
  PERSISTS across shell calls, so a later `./target/release/chezzi` silently means the worktree's binary
  — use absolute paths for any main-vs-branch comparison.
- Unit tests live next to the code in `#[cfg(test)] mod tests`. A large suite is a directory with one
  file per topic (`src/checker/tests/`, `src/vm/tests/`, `src/vm/golden_tests/`, `src/parser/tests/`,
  `src/compiler/tests/`): shared helpers live in its `mod.rs`, and **a new test goes in the file that
  owns its topic, next to its siblings — never appended to the end of an unrelated file** (appends at
  the end of one 42k-line file were the source of most merge conflicts). No fitting file: add one.
- **Testing is HYBRID — write the test in Chezzi if you can, fall back to Rust only when you can't.**
  A test that asserts a program's **observable behavior** (a value, a collection, a fault message)
  belongs in the native suite **`tests/chz/`** as `test fn` + `assert` (`spec/` = language behavior,
  `stdlib/` = std modules, `suites/` = struct suites with lifecycle hooks). It runs via `chezzi test`
  and is gated by the `cargo test` gate `chz_suite_passes` (`tests/chz_suite.rs`, its own process —
  `vm::pool` is one process-wide `OnceLock`), which runs the whole suite and asserts every test
  passes; `tests/chezzi_threads_cli.rs` then runs it again at `CHEZZI_THREADS=2`. Prefer this: it dogfoods the language and shrinks the Rust test surface. **Fault-path IS
  Chezzi-able** — `r := recover: <faulting expr>` then `assert r` is `!e` and check `e.message()`
  (don't reach for Rust just because a test expects a panic). **Fall back to Rust `#[cfg(test)]` ONLY
  for what `assert` genuinely can't express:** compile-time checker diagnostics (`rejects`/`ok`),
  token/AST/bytecode/GC internals, gc-stress rooting (`run_capture_stress`), and concurrency
  timing/scheduler behavior. Golden `examples/*.chz` + `.expected` stay fine for print-shape demos:
  `tests/examples_golden.rs` runs EVERY example that has a `.expected` (discovered from disk); an
  example whose prints race declares `# golden: unordered` in its header, one that pins stderr
  `# golden: stderr`.
  When you delete a Rust behavioral test after porting, the `tests/chz` gates must stay green. Full
  rationale + ranked runner follow-ups: `docs/future.md §3b`; the suite's own guide:
  `tests/chz/README.md`.
- **ONE ENGINE.** The bytecode VM on its M:N scheduler is the sole engine — the tree-walk interpreter
  and the cooperative `--serial` VM are both **removed** (`--serial` since 2026-08-16). There are no
  per-engine test-helper pairs any more: `run_capture`/`run_program`/`run_file` (and the golden
  helpers — `src/vm/golden_tests/`, `assert_golden_out`/`golden_entry*`, renamed from the historical
  `parity_tests.rs`/`assert_parity*` on 2026-08-16) all run the one engine and compare against a
  **literal golden**. A helper that runs the program twice and diffs the two runs against each other proves
  nothing — give it a real expectation.
- **A CHECKER WARNING'S GATE IS DERIVED FROM THE RUNTIME, ONE PROGRAM PER SHAPE — never from the
  plausible-looking checker flag.** The checker has a non-fatal `Severity::Warning` channel
  (`Checker::warn`, `warns`/`no_warn` test helpers) and a warning's whole value is that it is TRUE
  where it fires. So before writing the predicate, enumerate the positions the rule could fire in and
  **run each one** — the warn/silent split is a measured table, not a design. Both rules on that
  channel got their gate wrong on the first attempt in exactly this way: W8-2's filed prescription
  ("a discarded carrier is a compile error at top level") was false because the RUNTIME then aborted
  there, and its recommended escape `_ := g()` turned rc=1 into rc=0 — TICKET-038 later reversed the
  runtime abort itself for exactly that reason, so the warning fires at top level too now; the airlock rule's filed
  "locals only, `is_local_capture` draws exactly this line" would not have warned on its own filed
  repro, because at module top level the binding is scope 0 (`is_captured` is the real question). Two
  corollaries: derive the gate from an **exhaustive** enumeration of the compiler seam that decides it
  (W8-2's is the eight `FnComp::new` sites), and when unsure **decline** — an under-warn is a ceiling
  you can pin with a test, an over-warn teaches users to ignore the channel.
- **CORRECTNESS IS JUDGED AGAINST THE ANCESTOR — there is no engine agreement to hide behind.**
  With one engine there is no cross-engine oracle at all, so "both engines agree" is not merely a weak
  defense, it is not available. Whenever behavior is in question, judge it against the ancestor that
  owns the feature and **run the reference program** rather than reasoning about it: **Go** for
  concurrency/interfaces, **Python** for scripting feel + `Executor`-family semantics, **Rust** for
  enums/errors/control flow. If Chezzi disagrees with the owning ancestor and the difference isn't a
  documented deliberate decision, that is a **bug**. State the ancestor's measured output; never
  defend a behavior by citing another run of Chezzi. What replaced the cross-engine detector for
  ACCIDENTAL divergence: the CPython differential (`src/difftest/`) and the two-worker-count schedule
  differential (`tests/chezzi_threads_cli.rs`) — see `docs/bug-discovery.md` Tier 2. Corollary for any
  heuristic verdict (deadlock detection, resource caps, inference fallbacks): when unsure it must
  **decline** (hang / stay silent / ask), never emit a confident wrong answer — a missing answer is
  recoverable, a wrong one teaches distrust of every answer. Worked example + measured Go/CPython
  table: `docs/gaps-archive.md` **W7-12**.
