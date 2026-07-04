# Chezzi — Claude Code Guide

Chezzi is a fast, statically-typed, Python-feel scripting language, hand-built in Rust.
Full design + roadmap: **[`docs/spec.md`](docs/spec.md)**. Syntax cheat-sheet: **[`docs/syntax.md`](docs/syntax.md)**. Stdlib/builtin reference: **[`docs/stdlib.md`](docs/stdlib.md)**. Canonical grammar: **[`docs/grammar.bnf`](docs/grammar.bnf)** (executed + drift-checked by `cargo test conformance`). Progress tracker: **[`PROGRESS.md`](PROGRESS.md)**. Bug-discovery strategy (fuzzing / CPython differential / sanitizers, pre-JIT): **[`docs/bug-discovery.md`](docs/bug-discovery.md)**. The docs are also emitted by the CLI: `chezzi docs <topic>` / `chezzi docs` (full LLM bundle).

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

## Workflow per milestone

1. Implement the milestone (types, logic, wiring) in its module.
2. Add unit tests + a golden check against `examples/*.chz`.
3. `cargo test` + run the milestone's `chezzi` subcommand to verify end-to-end.
4. Update `PROGRESS.md`, commit, move on.

## Commands

```sh
cargo build --release    # compile (release; the VM is only fast optimized)
# Tests: `src/main.rs` (the `chezzi` CLI) is a thin shim over the `chezzi` library crate (src/lib.rs,
# the front-end + editor tooling), so the front-end compiles ONCE and its unit tests + two-engine
# parity + conformance run ONCE (in the lib test target). `cargo test` is the normal full command.
cargo test                       # FULL pre-commit suite: lib unit suite + parity + conformance + integration
cargo test --lib                 # INNER LOOP: just the lib unit suite (unit + two-engine parity + conformance, no integration/bin)
cargo test --lib checker::       # scope to the area you're editing → seconds (use while implementing)
cargo test --features lsp --test lsp_smoke   # the feature-gated LSP server smoke test (off the default build)
# Local shortcut: `.cargo/config.toml` (git-excluded) aliases these as `cargo tfast` / `tfull` / `tlsp`.
cargo test conformance   # execute docs/grammar.bnf, differential-test vs the parser
cargo clippy -- -D warnings   # lint (must be clean before commit)
cargo run -- help        # CLI usage

cargo run -- init my_proj                # scaffold a new project (chezzi.toml w/ entrypoint="src.main:main" + src/main.chz + a _test.chz)
cargo run -- tokens examples/hello.chz   # token stream (M1)
cargo run -- ast    examples/hello.chz   # parsed AST (M2)
cargo run -- check  examples/hello.chz   # type-check only (M4); --errors=json for machine output
cargo run -- run    examples/hello.chz   # type-check + run on the VM, OS-thread engine (default, M5)
cargo run -- run                         # no file → run the manifest [project] entrypoint (walks up for chezzi.toml)
cargo run -- run --serial   examples/hello.chz   # cooperative single-thread VM (the byte-identical parity oracle for the default M:N engine)
cargo run -- run --parallel examples/primes_parallel.chz   # accepted no-op alias (engine is now default)
cargo run -- run --threads=4 examples/primes_parallel.chz  # size the OS-thread pool (0/omitted = all cores; env CHEZZI_THREADS)
cargo run -- test examples/              # run every `test fn` in *_test.chz (M20); file or dir, default cwd
cargo run -- docs                        # print docs: no topic = full LLM reference bundle; `docs <topic>` = one (spec/syntax/stdlib); `docs topics` lists them
cargo run -- repl                        # interactive REPL (NOT IMPLEMENTED — stub errors; see src/main.rs:81)

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
> `chezzi run` defaults to the VM's real-thread OS-thread (M:N) engine. `--serial` selects the
> cooperative single-thread VM — the byte-identical **parity oracle** for the default M:N engine
> (both are the same `Vm`, toggled by the `parallel` flag; only the scheduler differs). `--parallel`
> is kept as a no-op alias for the default.
> `--threads=N` (or env `CHEZZI_THREADS`) sizes the OS-thread engine's worker pool — `0`/omitted = all
> cores, the flag wins over the env, and it errors with `--serial` (not multi-threaded).
> `--parallel`/`--serial` are mutually exclusive. (The tree-walk interpreter has been **removed**; the
> two-engine parity tests now assert serial-VM == M:N-VM.)

## Conventions

- Commits: single-line conventional (`feat:`, `fix:`, `chore:`, `docs:`, `test:`). No body.
- Each compiler phase is its own module under `src/`: `lexer` → `parser` → `ast` →
  `desugar` → `checker` → `compiler` → `vm` (the engine of record; its `impl Vm` is split across
  `vm/{exec,arith,call,sched,netio,stmt}.rs`), plus `gc`, `native` + `runtime` (builtins / std),
  `resolver` (module paths). (The tree-walk `interp` engine has been removed.)
- Keep modules small and single-purpose.
- **New builtin types/ctors/fns go in their owning `std.*` module (import-gated), NOT the global
  reserved namespace.** The global surface stays minimal: scalars, `tuple`, `range`, `Channel`,
  `Result`/`Option`/`Iterator`, structural protocols. (`timer(ms)` moved to `import std.time`; `Shared`/
  `RwShared`/`Atomic`/`Executor` to `import std.concurrency` — all stay reserved names.) Register in the
  module's `native_module_sig` and gate the bare name behind `import` via the per-module licensing set
  (mirror FFI `imported_ffi_types` / `imported_concurrency` / `imported_time`); keep runtime ctor/opcode
  dispatch unchanged (the gate is checker-only name resolution). A pure type/ctor with no runtime module-member value also needs the `bind_import` skip in
  the vm, or `from M import X` faults at runtime — cover it with a test that RUNS the program (serial + M:N).
  A global reserved name is a one-way ratchet: moving it out later breaks every example + grammar.bnf.
- After merging an auto-task branch (post-gate ships): delete the branch + prune its worktree
  (`git worktree remove --force <wt>; git worktree prune; git branch -D <branch>`). Stale worktree
  `target/` dirs (~1.6G each) accumulate and fill the disk. Delete rejected branches too.
- Unit tests live next to the code in `#[cfg(test)] mod tests`.
- **Tree-walk interpreter REMOVED.** The bytecode VM is the sole engine. Two-engine parity is now
  **serial-VM (`parallel=false`) == M:N-VM (`parallel=true`)** — both are the same `Vm`, only the
  scheduler differs. Test helpers: `run_capture`/`run_program`/`run_file` are the serial engine;
  `run_capture_parallel`/`run_program_parallel`/`run_file_p` are the M:N oracle. Keep them in sync —
  a VM change that diverges serial vs M:N is a bug.

## Where things stand

Core language is **implemented through M21 (and still evolving; M19 perf in progress)** (scalars, `List`/`Map`/`Set`/`tuple`,
generic structs + enums, `Result`/`Option` + `?`, generics + structural protocols,
exhaustive `match` + guards, closures/HOF, modules, GC, interpolation, pipe, `defer`,
`recover:`, `Iterator[T]`, slicing/indexing protocols). **Concurrency** has landed through
**Tier-D** (`spawn` / `parallel:` nursery, `Channel[T]`, `Shared[T]`, `Executor`, real
OS-thread engine via `--parallel`, netpoller + `std.net`). ~1500 tests green.

## Current focus

See **[`PROGRESS.md`](PROGRESS.md)** — single source of truth for "what's next."

Right now: **M19 — Perf track (in progress).** This milestone is performance-focused, so while perf
work is in flight the bar is **behavior-preserving + two-engine parity** on every change —
a VM speedup that diverges between the serial and M:N engines (or changes observable output) is a bug, not a win.
(The language is still evolving — new features can land; they just go through their own milestone,
not silently inside a perf change.)
Landed: peephole/const-fold, superinstructions, `invoke_value` clone-kill, in-place call args,
stringify-into-buffer, global-slotting, `ConstStr` interning, struct-field IC, FxHash, call-loop
flatten, small-string optimization (SSO). Current gap to CPython: **~1.3×–3.5×** slower (worst on
call-bound `fib` 3.54×; `loop` 1.32× is at the dispatch floor), startup ~11× **faster**.

**Next perf batch (ranked, not started — start here next session).** The remaining gap is **call
overhead + per-op dispatch + a few alloc paths**, not the value model or GC; target is CPython 3.14
(specializing interpreter + optional JIT). Do **Tier 1 in order**: (1) **method-call IC + flatten
`do_method_call`** (hits `struct`/OO), (2) **trim per-op overhead in `run_until`** (lazy `span`, split
serial-vs-MN loop, inline the hottest ops — hits `loop`/`primes`), (3) **call-site specialization for
`Op::Call`** (hits `fib`). Then Tier 2 (adaptive opcode quickening, PEP 659) → Tier 3 (Cranelift
method-JIT). Full ranked detail + `file:line`s in **[`docs/future.md §4` "Post-M19 next levers"]**
and **[`PROGRESS.md` "Next perf batch"]**. Same discipline: measure (`benches/run.chz`) → failing-then-
green correctness test → keep parity → re-measure → record the delta in `docs/benchmarks.md`.

**How to work a perf task here:** measure first (`cargo run --release -- run benches/run.chz`), land
behind a failing-then-green correctness test, keep parity green, re-measure, record the delta in
`docs/benchmarks.md` + `PROGRESS.md`. Don't trust a lever's a-priori payoff guess — several in the
backlog moved a *different* bench than predicted (logged in `docs/benchmarks.md`).

**Remaining levers** (ranked in **[`docs/future.md`](docs/future.md)** §4): Medium — NaN-boxing
`Value` (16B→8B, the biggest remaining lever; its own milestone), struct-field inline caching, string
concat/`split` builder. Big/separate — register VM, generational/incremental GC, and **Cranelift
AOT/JIT as the stretch end-game** (a whole backend — a late-stage endeavor once the language has
matured; not next). Frame pooling and general arith-specialization are deprioritized (superinstructions already
cover the hot int paths; `CallFrame`'s per-call `Vec`s are alloc-free).
