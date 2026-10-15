# Build times

The refactor (`docs/owners.md`, file splits, crate split) is judged by these numbers. Re-run the
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
Stage 2 (cut that cycle into a front-end crate and a VM crate over a shared core crate) is the step
that can cut a one-line release rebuild substantially; it is not done (`docs/owners.md`, PROGRESS).

- Release with `CARGO_PROFILE_RELEASE_INCREMENTAL=true`: cold 68 s, first real edit **36.8 s**.
  Not adopted: incremental release splits codegen into more units, which can slow the VM; it
  needs a benchmark run (`docs/benchmarks.md`) before it can be the default.
