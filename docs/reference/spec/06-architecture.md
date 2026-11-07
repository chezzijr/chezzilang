## Architecture — pipeline

```
source.chz
  → Lexer        (indent-aware: INDENT/DEDENT tokens)
  → Parser       (Pratt expr parsing + recursive-descent stmts) → AST
  → Desugar      (AST → AST lowering: comprehensions, defaults, interpolation)
  → Checker      (local inference; explicit fn sigs; machine-readable errors) → typed AST
  → Bytecode compiler → Stack VM (+ mark-sweep GC)
```

The desugar pass does not lower `?.`/`??`: they survive to the checker, which picks the lowering by
operand type (`lower_carrier_option`/`lower_carrier_try`/`lower_carrier_result_coalesce`; the compiler
calls the same functions). Each component is an isolated, separately-testable module, and the bytecode
VM is the only engine. Golden tests assert the VM's output against a literal expectation. For the file
that owns each area, see [`owners.md`](../../contributing/owners.md).

## Roadmap

Milestones, status and what comes next: [`PROGRESS.md`](../../../PROGRESS.md).

## Verification

- **Per-phase Rust unit tests**: lexer token streams, parser AST shapes, checker accept/reject cases.
- **Golden tests**: `examples/*.chz` + `*.expected`; the harness runs each on the VM and asserts stdout matches the `.expected` file.
- **Manual end-to-end** through the `chezzi` CLI subcommand for each phase (`tokens`/`ast`/`run`).
- **LLM-codegen eval**: feed the grammar cheatsheet + `--errors=json` to a model and measure the first-try compile rate; failures feed grammar/error-message work.
- **Perf check**: tracked against CPython with the `benches/` harness (`benches/run.chz`, hyperfine);
  baseline + per-bench bottleneck analysis in [`benchmarks.md`](../../contributing/benchmarks.md).
  Chezzi measured ~1.3×–3.9× slower than CPython (worst on call/alloc-bound benches), with startup ~11× faster.
