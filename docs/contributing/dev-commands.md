# Development commands

The full command reference, moved out of `CLAUDE.md` (which every agent turn reads). `CLAUDE.md` keeps the core build/test/lint lines.

```sh
cargo build --release    # compile (release; the VM is only fast optimized)
# Tests: each layer is a crate (`crates/chezzi-{syntax,native,base,front,vm}`, sources under `src/`);
# the root `chezzi` crate holds the CLI, `run` and the editor. `cargo test` is the normal full command.
cargo test                       # FULL pre-commit suite: lib unit suite + goldens + conformance + integration
cargo test -p chezzi-front       # one crate's tests (also -p chezzi-syntax / chezzi-native / chezzi-base / chezzi-vm)
# ^ `cargo test` includes `tests/chezzi_threads_cli.rs`: the standing schedule differential is
#   `tests/chz` (the Chezzi behavioural suite) run at TWO worker counts (default + `CHEZZI_THREADS=2`)
#   via the built binary, each its own process/pool. NOT a gate over the Rust lib tests — that
#   pool is ONE process-wide `OnceLock`, so forcing a count inside `cargo test --lib` either no-ops or
#   (worse) pins the WHOLE run's pool and starves concurrently-running tests (measured: 8
#   failures/hangs at `RUST_TEST_THREADS=4`, >54 min unfinished at `=1`) — don't re-attempt an
#   in-process version of this gate. Also NOT `docs/design/roadmap.md` §2b's Go-paired-programs differential,
#   which is still unbuilt and separately planned (the seeded/interleaving M:N mode shipped under
#   TICKET-167 — see `CHEZZI_SCHED_SEED` and `schedfuzz` below).
cargo nextest run               # SAME full suite, test binaries in parallel (~3 min vs ~13; config .config/nextest.toml; the pipeline's verifying gate uses this)
# Test time: a test finishes well under 100 s, a third of nextest's 300 s `slow-timeout` (60 s x 5),
#   so two suites at once on one box do not push it past the limit. `.config/nextest.toml` owns the
#   300 s number. Nextest prints `SLOW [> 60.000s]` in the gate log for a test past 60 s, so a
#   growing test shows there before it times out. Split a slow grid into one test per row with
#   `grid_tests!` (`tests/support/grid_tests.rs`), or make its slow case faster. Never raise the
#   limit or add a per-test `slow-timeout` override. `tests/test_time_budget.rs` fails on a
#   `slow-timeout` line without `terminate-after = 5 }`.
cargo test --lib                 # INNER LOOP: just the lib unit suite (unit + goldens + conformance, no integration/bin)
cargo test --lib checker::       # scope to the area you're editing → seconds (use while implementing)
cargo test --features lsp --test lsp_smoke   # the feature-gated LSP server smoke test (off the default build)
# Local shortcut: `.cargo/config.toml` (git-excluded) aliases these as `cargo tfast` / `tfull` / `tlsp`.
cargo test conformance   # execute docs/reference/grammar.bnf, differential-test vs the parser
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
CHEZZI_SCHED_SEED=<u64> cargo run -- run <file>   # seeded scheduler mode (TICKET-167): replay at T=1 (byte-for-byte for body channel waits and a callback `send`; W15-10 lists the rest), perturbation at T>=2; a failing run reports its seed on stderr
CHEZZI_GC_STRESS=1 cargo run -- run <file>   # request a collection at every instruction boundary (`run` and `test`); each one stops every thread of the run, so a wait that keeps its GC token hangs and a missing GC root faults
CHEZZI_HEAP_STATS=1 cargo run -- run <file>  # after the run, on stderr: `[heap-stats] peak_live_bytes=<n> size_of_value=<n> stw=<stops of the world> stw_max_us=<longest stop, from the request until the world restarts, the collection included>`
cargo build --release --bin schedfuzz --bin chezzi && target/release/schedfuzz --seeds 1..33 --threads 1,2,0   # long-running seeded-scheduler sweep over tests/chz + concurrency examples/*.chz (0 = default worker count too — W15-1 only reproduces there); see docs/contributing/bug-discovery.md "Seeded scheduler oracle"
cargo run -- docs                        # print docs: no topic = full LLM reference bundle; `docs <topic>` = one (spec/syntax/stdlib); `docs topics` lists them

cargo run -- run benches/run.chz         # Chezzi-vs-CPython bench harness (see docs/contributing/benchmarks.md)

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
> wins over the env.
