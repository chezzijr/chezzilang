# `tests/chz/` — native Chezzi test suite

Behavior tests written **in Chezzi**, using the language's own `test fn` + `assert` (M20). This is
the dedicated home for dogfooded behavior specs — kept separate from `examples/` (which holds
print-and-golden demo programs, `*.chz` + `*.expected`).

## Layout

- `spec/` — language behavior (list/str/map/set/control-flow/…), ported from the Rust string-goldens
  in `src/vm/tests.rs` and `src/vm/parity_tests.rs`.
- `stdlib/` — stdlib module behavior (math/encoding/crypto/…), ported from `src/native/*`.
- `suites/` — `struct`-based test suites with lifecycle hooks + shared fixtures.

## Run

```sh
cargo run -- test tests/chz/            # run the whole suite (M:N engine, default), PASS/FAIL + summary
cargo run -- test --serial tests/chz/   # cooperative single-thread VM instead
cargo run -- test tests/chz/spec/list_test.chz   # one file
```

## The gate (why this replaces Rust `parity_*` tests)

A single `chezzi test` invocation runs **one** engine (M:N by default, `--serial` to switch). The Rust
tests these are ported from also asserted **serial VM == M:N VM** (byte-identical); that comparison is
gone now that `--serial` is not the engine of record. What remains is the `cargo test` gate
`test_runner::chz_suite_passes`, which runs this entire suite on the M:N engine and asserts every
test passes. So `cargo test` — not just `chezzi test` — is the authoritative gate.

## What stays in Rust (not portable)

**Fault paths DO port.** `recover:` catches a fault into a `Result`, so a fault's *message* is
assertable in Chezzi (empty `min()`, OOB indexing, overflow, bad chunk size):

```chezzi
r := recover: [].min()
match r:
    Ok(_):  assert false, "expected a fault"
    Err(e): assert e.message().contains("min")
```

What genuinely can't port: compile-time checker tests (`rejects`/`ok`), parser/lexer (AST/token
shapes), compiler/bytecode/GC internals, and concurrency timing/scheduler parity. Value/collection
comparisons and fault-message checks port here; the rest stays in Rust.

## Adding tests

Mirror an existing file. Free tests: `test fn name(): assert <expr>[, "msg"]`. Suites: a `struct`
with `test fn name(self)` methods + optional `before_all`/`after_all`/`before_each`/`after_each`
hooks — see `suites/suite_test.chz`. Keep every assertion a deterministic value comparison.
