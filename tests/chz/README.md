# `tests/chz/` — native Chezzi test suite

Behavior tests written **in Chezzi**, using the language's own `test fn` + `assert` (M20). This is
the dedicated home for dogfooded behavior specs — kept separate from `examples/` (which holds
print-and-golden demo programs, `*.chz` + `*.expected`).

## Layout

- `spec/` — language behavior (list/str/map/set/control-flow/…), ported from the Rust string-goldens
  in `src/vm/tests.rs` and `src/vm/golden_tests.rs`.
- `stdlib/` — stdlib module behavior (math/encoding/crypto/…), ported from `src/native/*`.
- `suites/` — `struct`-based test suites with lifecycle hooks + shared fixtures.
- `go/` — ports of ancestor test suites (Go `test/chan`): faithful, one file per source file, the
  source's license in the dir's `NOTICE`.

## Run

```sh
cargo run -- test tests/chz/                     # run the whole suite, PASS/FAIL + summary
CHEZZI_THREADS=2 cargo run -- test tests/chz/    # ...again at a second worker count
cargo run -- test tests/chz/spec/list_test.chz   # one file
```

## The gate (why this replaces Rust `parity_*` tests)

The bytecode VM on its M:N scheduler is the **only** engine, so every test asserts against a literal
expectation, never against a second run. Two `cargo test` gates carry the suite:

- **`chz_suite_passes`** (`tests/chz_suite.rs`) runs this entire suite and asserts every test passes.
- **`chezzi_threads_cli`** (`tests/chezzi_threads_cli.rs`) runs it again at `CHEZZI_THREADS=2` — a
  *second schedule*, the accidental-divergence detector (`docs/contributing/bug-discovery.md` Tier 2).

A schedule-dependent failure replays under a fixed seed: `CHEZZI_SCHED_SEED=<u64>` (seeded
scheduler mode; a failing run reports its seed on stderr, see `docs/contributing/dev-commands.md`).

Both live in `tests/`, not the lib unit suite: `vm::pool` is one process-wide `OnceLock`, so a test
that needs uncontended pool workers must have its own process. So `cargo test` — not just
`chezzi test` — is the authoritative gate.

## What stays in Rust (not portable)

**Fault paths DO port.** `recover:` catches a fault into a `T!E`, so a fault's *message* is
assertable in Chezzi (OOB indexing, overflow, bad chunk size):

```chezzi
r := recover: [1, 2].chunk(0)
match r:
    ?_:  assert false, "expected a fault"
    !e: assert e.message().contains("chunk")
```

(Not every empty-collection case is a fault: `min`/`max`/`min_by`/`max_by`, like `first`/`last`/`pop`,
return `T?` and answer `None` — assert that directly, no `recover:`.)

What genuinely can't port: compile-time checker tests (`rejects`/`ok`), parser/lexer (AST/token
shapes), compiler/bytecode/GC internals, and concurrency timing/scheduler parity. Value/collection
comparisons and fault-message checks port here; the rest stays in Rust.

## Adding tests

Mirror an existing file. Free tests: `test fn name(): assert <expr>[, "msg"]`. Suites: a `struct`
with `test fn name(self)` methods + optional `before_all`/`after_all`/`before_each`/`after_each`
hooks — see `suites/suite_test.chz`. Keep every assertion a deterministic value comparison.
