# Chezzi — Claude Code Guide

Chezzi is a fast, statically-typed, Python-feel scripting language, hand-built in Rust.
Full design + roadmap: **[`docs/spec.md`](docs/spec.md)**. Syntax cheat-sheet: **[`docs/syntax.md`](docs/syntax.md)**. Stdlib/builtin reference: **[`docs/stdlib.md`](docs/stdlib.md)**. Canonical grammar: **[`docs/grammar.bnf`](docs/grammar.bnf)** (executed + drift-checked by `cargo test conformance`). Progress tracker: **[`PROGRESS.md`](PROGRESS.md)**. Bug-discovery strategy (fuzzing / CPython differential / sanitizers, pre-JIT): **[`docs/bug-discovery.md`](docs/bug-discovery.md)**. Hard-won engineering rules (distilled from working memory — read the section for the area you touch): **[`docs/lessons.md`](docs/lessons.md)**. The docs are also emitted by the CLI: `chezzi docs <topic>` / `chezzi docs` (full LLM bundle).

## How we work

Claude implements directly. Ship working, tested code each session.

- Write real implementations, not `todo!()` stubs.
- Every milestone lands with passing tests and a clean `cargo build` / `cargo clippy`.
- Each compiler phase is its own module under `src/`. Keep modules focused.
- Verify before claiming done: run the tests and the relevant `chezzi` subcommand, show real output.
- Keep `PROGRESS.md` current after each session; commit in conventional, single-line messages.
- **Docs are part of the change, not a follow-up.** When a change adds/alters/removes observable
  behavior — syntax, stdlib/builtin surface, engine semantics, a flag, a limit lifted/added, or a perf
  delta — update the affected `.md` in the **same commit**: `PROGRESS.md` always, plus as relevant
  `docs/syntax.md`, `docs/spec.md`, `docs/grammar.bnf`, `docs/concurrency*.md`, `docs/future.md`,
  `docs/benchmarks.md`. Grep the docs for any now-stale claim about what you changed (`deferred`,
  `experimental`, `not yet`, `follow-up`, `mooted`) and fix or delete it. A green build with stale docs
  is not done.
- Match the existing code's style and patterns; reuse before adding new abstractions.
- **Derive, don't mirror.** Every fact has one owner and everything else reads it. Before fixing a
  bug, ask **"which duplicate does this delete, or which single source does it add?"** — never add
  another copy, clause or veto next to the failing site in a family that already has sibling fixes.
  A new "Mirrors the checker's …" / "keep in sync with …" comment, or a drift test with an exemption,
  is a red flag. Family fixes land with one test that enumerates the whole grid. Why and where:
  `docs/lessons.md` §1, `docs/root-causes-w16.md`.

## Workflow per milestone

1. Implement the milestone (types, logic, wiring) in its module.
2. Add tests — **prefer Chezzi** (`tests/chz/`, see the testing-policy convention below); Rust
   `#[cfg(test)]` only for what `assert` can't express; a golden `examples/*.chz` for print-shape demos.
3. `cargo test` + run the milestone's `chezzi` subcommand to verify end-to-end.
4. Update `PROGRESS.md`, commit, move on.

## Commands

```sh
cargo build --release    # compile (release; the VM is only fast optimized)
# Tests: `src/main.rs` (the `chezzi` CLI) is a thin shim over the `chezzi` library crate (src/lib.rs,
# the front-end + editor tooling), so the front-end compiles ONCE and its unit tests + goldens +
# conformance run ONCE (in the lib test target). `cargo test` is the normal full command.
cargo test                       # FULL pre-commit suite: lib unit suite + goldens + conformance + integration
# ^ includes `tests/chezzi_threads_cli.rs`: with `--serial` gone, the standing differential is
#   `tests/chz` (the Chezzi behavioural suite) run at TWO worker counts (default + `CHEZZI_THREADS=2`)
#   via the built binary, each its own process/pool. NOT a gate over the Rust lib tests — that
#   pool is ONE process-wide `OnceLock`, so forcing a count inside `cargo test --lib` either no-ops or
#   (worse) pins the WHOLE run's pool and starves concurrently-running tests (measured: 8
#   failures/hangs at `RUST_TEST_THREADS=4`, >54 min unfinished at `=1`) — don't re-attempt an
#   in-process version of this gate. Also NOT `docs/future.md` §2b's Go-paired-programs differential,
#   which is still unbuilt and separately planned (the seeded/interleaving M:N mode shipped under
#   TICKET-167 — see `CHEZZI_SCHED_SEED` and `schedfuzz` below).
cargo nextest run               # SAME full suite, test binaries in parallel (~3 min vs ~13; config .config/nextest.toml; the pipeline's verifying gate uses this)
cargo test --lib                 # INNER LOOP: just the lib unit suite (unit + goldens + conformance, no integration/bin)
cargo test --lib checker::       # scope to the area you're editing → seconds (use while implementing)
cargo test --features lsp --test lsp_smoke   # the feature-gated LSP server smoke test (off the default build)
# Local shortcut: `.cargo/config.toml` (git-excluded) aliases these as `cargo tfast` / `tfull` / `tlsp`.
cargo test conformance   # execute docs/grammar.bnf, differential-test vs the parser
cargo clippy -- -D warnings   # lint (must be clean before commit)
cargo run -- help        # CLI usage
cargo run -- version     # print the toolchain version (also --version / -V)

cargo run -- init my_proj                # scaffold a new project (chezzi.toml w/ entrypoint="src.main:main" + src/main.chz + a _test.chz)
cargo run -- tokens examples/hello.chz   # token stream (M1)
cargo run -- ast    examples/hello.chz   # parsed AST (M2)
cargo run -- check  examples/hello.chz   # type-check only (M4); --errors=json for machine output
# ^ json objects are {file?, line, col, end_line, end_col, severity, message, help?}; `file` is
#   OMITTED when the span has no module coordinate (never a claimed-wrong path); severity is "error"
#   or "warning" (a warning is non-fatal — reported, exit code unchanged); `help` is a "did you mean
#   '<name>'?" near-miss suggestion on a method/field/unknown-name/module-member/enum-variant miss,
#   OMITTED when there is none
cargo run -- run    examples/hello.chz   # type-check + run on the VM, OS-thread engine (default, M5)
# ^ --errors=json puts warnings AND errors in the one check-shaped array, as stdout's FIRST line,
#   ahead of the program's own output; plain-text `run` keeps warnings on stderr, unchanged
cargo run -- run                         # no file → run the manifest [project] entrypoint (walks up for chezzi.toml)
cargo run -- run --parallel examples/primes_parallel.chz   # accepted no-op alias (the M:N engine is the only engine)
cargo run -- run --threads=4 examples/primes_parallel.chz  # size the OS-thread pool (0/omitted = all cores; env CHEZZI_THREADS)
cargo run -- test examples/              # run every `test fn` in *_test.chz (M20); file or dir, default cwd
CHEZZI_THREADS=4 cargo run -- test tests/chz   # `test` sizes the same pool as `run`, env-only (no `--threads` flag on `test`)
CHEZZI_SCHED_SEED=<u64> cargo run -- run <file>   # seeded scheduler mode (TICKET-167): replay at T=1 (byte-for-byte for channel waits; W15-10 lists the rest), perturbation at T>=2; a failing run reports its seed on stderr
cargo build --release --bin schedfuzz --bin chezzi && target/release/schedfuzz --seeds 1..33 --threads 1,2,0   # long-running seeded-scheduler sweep over tests/chz + concurrency examples/*.chz (0 = default worker count too — W15-1 only reproduces there); see docs/bug-discovery.md "Seeded scheduler oracle"
cargo run -- docs                        # print docs: no topic = full LLM reference bundle; `docs <topic>` = one (spec/syntax/stdlib); `docs topics` lists them

cargo run -- run benches/run.chz         # Chezzi-vs-CPython bench harness (see docs/benchmarks.md)

cargo install --path . --features lsp --bin chezzi-lsp        # editor LSP server → ~/.cargo/bin (neovim/lvim setup: editors/README.md)
# ^ RE-RUN this after ANY lexer/checker/grammar change: the installed binary is a snapshot, so
#   editors keep serving stale diagnostics/highlighting until you reinstall (it's the auto-extend seam).
cargo build --features lsp --bin chezzi-lsp                    # …or build in place → target/debug/chezzi-lsp (off the default build)
UPDATE_EDITOR_ASSETS=1 cargo test --test editor_tmlanguage    # regenerate the VSCode TextMate grammar (single-sourced from the lexer)
```

> Flags go **before** the file path; anything after the file is passed to the program.
> `chezzi run` with NO file argument runs the project manifest's `[project] entrypoint` (a dotted
> module path with an optional `:function` suffix, e.g. `"src.main:main"`): the project root is found
> by walking up from the cwd for `chezzi.toml`, the module resolves root-relatively and runs
> top-to-bottom, and with a `:function` suffix that function is then called (missing/non-function =
> clear error). Without the suffix the module top-level runs only. `chezzi run <file>` runs that file
> (top-level only, scripting model).
> `chezzi run` runs the VM on its real-thread OS-thread (M:N) scheduler. That is the **only** engine;
> `--parallel` is kept as an accepted no-op alias for it.
> `--threads=N` (or env `CHEZZI_THREADS`) sizes the worker pool — `0`/omitted = all cores, and the flag
> wins over the env. (Both the tree-walk interpreter and the cooperative single-thread `--serial`
> engine have been **removed** — `--serial` and `--check-parity` are now `unknown flag` errors.)

## Conventions

- Commits: single-line conventional (`feat:`, `fix:`, `chore:`, `docs:`, `test:`). No body.
- Each compiler phase is its own module under `src/`: `lexer` → `parser` → `ast` →
  `desugar` → `checker` → `compiler` → `vm` (the engine of record; its `impl Vm` is split across
  `vm/{exec,arith,call,sched,netio,stmt}.rs`), plus `gc`, `native` (builtins / std),
  `resolver` (module paths). (The tree-walk `interp` engine has been removed.)
- Keep modules small and single-purpose.
- **New builtin types/ctors/fns go in their owning `std.*` module (import-gated), NOT the global
  reserved namespace.** The global surface stays minimal: scalars, `tuple`, `range`, `Channel`,
  `Result`/`Option`/`Iterator`, structural protocols. (`timer(ms)` moved to `import std.time`; `Shared`/
  `RwShared`/`Atomic`/`Executor` to `import std.concurrency` — all stay reserved names.) Register in the
  module's `native_module_sig` and gate the bare name behind `import` via the per-module licensing set
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
- Unit tests live next to the code in `#[cfg(test)] mod tests`.
- **Testing is HYBRID — write the test in Chezzi if you can, fall back to Rust only when you can't.**
  A test that asserts a program's **observable behavior** (a value, a collection, a fault message)
  belongs in the native suite **`tests/chz/`** as `test fn` + `assert` (`spec/` = language behavior,
  `stdlib/` = std modules, `suites/` = struct suites with lifecycle hooks). It runs via `chezzi test`
  and is gated by the `cargo test` gate `chz_suite_passes` (`tests/chz_suite.rs`, its own process —
  `vm::pool` is one process-wide `OnceLock`), which runs the whole suite and asserts every test
  passes; `tests/chezzi_threads_cli.rs` then runs it again at `CHEZZI_THREADS=2`. Prefer this: it dogfoods the language and shrinks the Rust test surface. **Fault-path IS
  Chezzi-able** — `r := recover: <faulting expr>` then `assert r` is `Err` and check `e.message()`
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
  helpers — `src/vm/golden_tests.rs`, `assert_golden_out`/`golden_entry*`, renamed from the historical
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

## Where things stand

Core language is **implemented through M24 (and still evolving; M19 perf in progress)** (scalars, `List`/`Map`/`Set`/`tuple`,
generic structs + enums, `Result`/`Option` + `?`, generics + structural protocols,
exhaustive `match` + guards, closures/HOF, modules (incl. Python full paths `a.b.X`, TICKET-175),
GC, interpolation, pipe, `defer`, `recover:`, `Iterator[T]`, slicing/indexing protocols,
user-overloadable `==` via `Eq`, static protocol requirements via witness passing). **Concurrency** has
landed through **Tier-D** (`spawn` / `parallel:` nursery, `Channel[T]`, `Shared[T]`, `Executor`, the real
OS-thread M:N engine, netpoller + `std.net`). The checker has a **non-fatal warning channel**
(`Severity::Warning`, `"severity"` in `--errors=json`, `DiagnosticSeverity::WARNING` in the LSP) with
four rules — a discarded `Result`/`Option`, a mutating call on a `Shared`/`RwShared`/`Atomic` read
temporary, an unreachable `match` arm, an unread local (TICKET-090). Test totals move with every ticket;
read the live counts from `cargo test` / `chezzi test tests/chz`, never from prose.

## Current focus

See **[`PROGRESS.md`](PROGRESS.md)** — single source of truth for "what's next."

Right now: **pre-JIT bug-hunt**, with **M19 — Perf track** paused alongside it.

> **START HERE (2026-09-28): read [`docs/root-causes-w16.md`](docs/root-causes-w16.md) before fixing
> anything in the core.** Bug-hunt wave 16 (JIT sweep #1) found 5 new P0 and 7 new P1, and traced them
> to five systemic families — **Names, Blocking, Declarations, Airlock, Identity** — each one fact
> decided in several places kept in sync by hand. An audit of TICKET-120..176 found 34 of 51 fixes were
> point patches that added one more copy/clause/veto, and every family patched that way came back.
> **Rule: derive, don't mirror** (`docs/lessons.md` §1); the pipeline's planning and plan-validation
> stages enforce a `## Single source` section for these families. Planned fix order: Identity →
> Declarations (types are top-level only, owner decision 2026-09-28) → Airlock → Names → Blocking, each
> with one test that enumerates its whole grid.
>
> **JIT ENTRY RULE (2026-09-22, full text `PROGRESS.md` "JIT entry rule"):** (1) seeded scheduler oracle
> built and judged — built, re-finds 2 of 3 reverted races, owner's call pending; (2) two consecutive
> sweeps with zero new P0/P1 in the core — **sweep #1 (wave 16) was not clean**; (3) no open P0/P1 row;
> (4) feature freeze for the window — not yet declared.
>
> **D4 (APPROVED 2026-09-22):** a spawned task's write to an airlock copy is an error or a runtime
> fault, never a silent lost write (layer C TICKET-169, layer A TICKET-170). Read
> `docs/decision-d4-airlock.md` before any airlock work, and `docs/root-causes-w16.md` Family 4 for
> where today's marking is too broad.
>
> **Ledger.** `docs/gaps.md` is a SHORT LEDGER of open rows, each re-verified on the release binary;
> every closed row and every wave's session log (W1..W15) lives verbatim in `docs/gaps-archive.md`, so
> a `docs/gaps.md:NNNN` citation in an old ticket resolves against the archive. **8 open rows** today —
> W8-19, W13-28, W15-4, W15-5, W15-8, W15-9, W15-10, W17-1, all P2/P3 or tracking rows (this count is
> gate-checked by `tests/gaps_ledger_count.rs`; re-derive it with `grep -c '^| \*\*W' docs/gaps.md`
> after every merge — merged counters can be silently wrong). Wave 16's findings are recorded in
> `docs/root-causes-w16.md` and are not yet ledger rows.
>
> Standing lessons from past waves, kept here because they still bite: the mechanical oracles
> (panic-fuzz, CPython differential) find almost nothing — findings come from hand-built programs judged
> against a RUN Go/Rust/CPython reference; sample a flaky rate (n >= 5 per side) before any causal claim;
> and read a closed row's *prescription* before re-implementing it — several filed fixes (W8-2, W8-7,
> W9-5) were measured wrong.

Both share the same bar: **behavior-preserving** on every change — a VM
speedup that changes observable output, or that only holds at one worker count, is a bug, not a win.
(Since 2026-08-16 there is no second engine to diff against; the standing accidental-divergence
detectors are the CPython differential and the two-worker-count run of `tests/chz`.)
(The language is still evolving — new features can land; they just go through their own milestone,
not silently inside a perf change.)
Landed: peephole/const-fold, superinstructions, `invoke_value` clone-kill, in-place call args,
stringify-into-buffer, global-slotting, `ConstStr` interning, struct-field IC, FxHash, call-loop
flatten, small-string optimization (SSO). Current gap to CPython: **~1.3×–3.5×** slower (worst on
call-bound `fib` 3.54×; `loop` 1.32× is at the dispatch floor), startup ~11× **faster**.

**Next perf batch (ranked, not started — start here next session).** The remaining gap is **call
overhead + per-op dispatch + a few alloc paths**, not the value model or GC; target is CPython 3.14
(specializing interpreter + optional JIT). Do **Tier 1 in order**: (1) **method-call IC + flatten
`do_method_call`** (hits `struct`/OO), (2) **trim per-op overhead in `run_until`** (lazy `span`,
inline the hottest ops — hits `loop`/`primes`), (3) **call-site specialization for
`Op::Call`** (hits `fib`). Then Tier 2 (adaptive opcode quickening, PEP 659) → Tier 3 (Cranelift
method-JIT). Full ranked detail + `file:line`s in **[`docs/future.md §4` "Post-M19 next levers"]**
and **[`PROGRESS.md` "Next perf batch"]**. Same discipline: measure (`benches/run.chz`) → failing-then-
green correctness test → keep the suite green at both worker counts → re-measure → record the delta in
`docs/benchmarks.md`.

**How to work a perf task here:** measure first (`cargo run --release -- run benches/run.chz`), land
behind a failing-then-green correctness test, keep the suite green, re-measure, record the delta in
`docs/benchmarks.md` + `PROGRESS.md`. Don't trust a lever's a-priori payoff guess — several in the
backlog moved a *different* bench than predicted (logged in `docs/benchmarks.md`).

**Remaining levers** (ranked in **[`docs/future.md`](docs/future.md)** §4): the 8-byte pointer-tagged
`Value` already shipped (NaN-boxing retired, blocked by full `i64`), and struct-field inline caching is
landed; string concat/`split` builder remains. Big/separate — register VM, generational GC (measured at most ~11% of runtime, `docs/benchmarks.md` 2026-09-28), and **Cranelift
AOT/JIT as the stretch end-game** (a whole backend — a late-stage endeavor once the language has
matured; not next). Frame pooling and general arith-specialization are deprioritized (superinstructions already
cover the hot int paths; `CallFrame`'s per-call `Vec`s are alloc-free).
