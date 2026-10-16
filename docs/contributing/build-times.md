# Build times

The refactor (`docs/contributing/owners.md`, file splits, crate split) is judged by these numbers. Re-run the
probe after each stage and add a row. Do not compare numbers from different machines or loads.

Probe: insert one comment line in the middle of the file (it shifts every span after it, as a
real edit does), build, restore the file. Median of 3 for dev rows, one run for release rows.
Target dir on `/home` (not tmpfs); `.cargo/config.toml` caps `jobs = 4`. Box: 28 threads, 15 GB.

## 2026-10-15 baseline (one crate, 238,695 lines in `src/`)

| Edited file | `cargo build` (dev) | `cargo test --lib --no-run` | `cargo nextest run --no-run` | `cargo build --release` |
|---|---|---|---|---|
| `checker/sig.rs` | 6.0 s | 10.0 s | 20.0 s | **67.3 s** |
| `checker/tests.rs` | 0.2 s | 8.7 s | | |
| `parser/mod.rs` | 4.0 s | 8.9 s | | |
| `vm/arith.rs` | 4.9 s | 9.8 s | | |
| `lexer/mod.rs` | 4.6 s | 9.6 s | | |

- A `touch` with no content change gives about the same dev numbers (4.1–4.2 s): dev incremental
  is already cheap. **The release build is the cost**: the pipeline gate and every `tests/chz`
  run build `--release --bin chezzi`, and release has no incremental cache.
## 2026-10-15 stage 1: `chezzi-syntax` crate (lexer, ast, parser, fmtspec, printable, slice)

| Edited file | `cargo build --release` |
|---|---|
| `checker/sig.rs` | 64.9 s (was 67.3) |
| `parser/expr.rs` | 66.9 s |
| `vm/arith.rs` | 66.6 s |

Stage 1 saves 3.5%: the syntax layer is about 21k of 238k lines, and the cost is the codegen of the
201k-line cycle that stays one crate (checker ↔ compiler ↔ vm ↔ native ↔ resolver ↔ desugar).
Stage 2 (below) cuts that cycle.

- Release with `CARGO_PROFILE_RELEASE_INCREMENTAL=true`: cold 68 s, first real edit **36.8 s**.
  Not adopted: incremental release splits codegen into more units, which can slow the VM; it
  needs a benchmark run (`docs/contributing/benchmarks.md`) before it can be the default.

## 2026-10-16 stage 2: six crates (`syntax` ← `native` ← `base` ← `front`, `vm` ← `chezzi`)

Interleaved probe: the pre-split commit (`98fafb22`, one crate plus `chezzi-syntax`) in a worktree
inside the repo and the split tree, one edit each in turn, so machine load hits both sides.

| Edited file | `cargo build --release`, before | after |
|---|---|---|
| `checker/sig.rs` | 63.5 s, 63.6 s | **45.3 s, 45.2 s** (−29%) |
| `vm/arith.rs` | 63.6 s, 63.1 s | **52.5 s, 36.6 s** (−17% to −42%) |

- A front-end edit no longer recompiles the VM, and a VM edit no longer recompiles the checker.
- `cargo test --workspace --no-run` after an edit is dominated by relinking the ~60 binaries in
  `tests/` against the `chezzi` crate, and on this box it varied 3–5× between back-to-back runs
  (19–111 s for one edit), so no before/after claim is made for it. One earlier run, not interleaved and so only a hint,
  measured a test-file edit at 6.0 s (`checker/tests/basics.rs`) and 4.7 s (`vm/tests/lang_basics.rs`)
  against 18.3 s and 11.6 s before. The scoped loop `cargo test -p chezzi-front --no-run` after a checker edit is
  8.2 s.

