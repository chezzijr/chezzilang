# Progress: bug-hunt wave 11 summary (moved verbatim from `PROGRESS.md` lines 9379-9411)

## Bug-hunt wave 11 (2026-09-08) — 13 findings, 6 tickets

Six parallel agents over disjoint domains + FFI/`std.net` in the main loop; every row re-verified by
the judging loop on the release binary at `699be0d2`, every candidate judged against a RUN Go / CPython
/ Rust reference. Index and full session log: the end of **[`docs/contributing/gaps.md`](docs/contributing/gaps.md)**
(`W11-1..W11-13`).

- **TICKET-093 (P0) — CLOSED 2026-09-08.** `fn`-type parameters compared covariantly; `check` clean,
  then an `int` sat in a `List[str]` at rc=0. Fixed by making parameters strictly INVARIANT (the
  Go/Rust rule, not contravariant) at all three sites — `src/checker/proto.rs:1233`,
  `src/checker/ty.rs:640` and `:667` — via a new `param_invariant` helper; the return stays covariant.
- **TICKET-094 (P1/P2 ×3, CLOSED 2026-09-08)** — expected-type/widening does not reach three declared
  sinks: an `internal:` compiler-bug abort on a mixed-numeric list DEFAULT (`span-keyed-table-aliasing`
  #5), the element widen missing at both default sinks, and a generic callee dropping the hint on
  concrete slots. Fixed by licensing `float_elem_hint` at both default decl-sites (`src/checker/sig.rs`)
  and threading the declared arg/param slot through `infer_generic_arg_tys`/`check_generic_arg`
  (`src/checker/expr.rs`, `src/checker/proto.rs`), keyed on the PRE-substitution slot per DEC-054. Rust
  lib suite: 4581 → 4586 passed, 0 failed. Chezzi suite (`tests/chz`): 853 passed, 0 failed (4 new
  cases in `tests/chz/spec/float_elem_widen_sinks_test.chz`).
- **TICKET-095 (P1)** — a nested-nursery deadlock hangs forever at `CHEZZI_THREADS=1`; Go faults at
  every `GOMAXPROCS`. Ungated because `tests/chezzi_threads_cli.rs` only runs default + T=2.
- **TICKET-096 (P1/P2)** — a module top-level `spawn` fault is delivered twice (caught AND fatal) with
  silent truncation after the handler; and `main`'s loop back-edge is not a cancel checkpoint (39 774 ms
  vs 33 ms one scope down).
- **TICKET-097 (P1)** — a crossing closure carries the sender's module global only for some spellings
  (`fill_global_free` never walks `Op::Call`; an in-place write never sets the `assigned` bit).
- **TICKET-098 (P3 ×4)** — an unspellable struct-match witness, a `std.cancel` i64 overflow, two
  `parse_int_base` divergences, a missing `std.regex` replacement diagnostic.

Clean columns worth not re-hunting: the whole `std.*` surface (~28 000 differential cases vs CPython
3.14 / Go 1.26, zero correctness bugs), the FFI null-guard set (all 32 forms), GC-under-callback and
the stored-callback abort net, and `std.net`'s sticky-UTF-8 + `read_bytes` drain contract.
