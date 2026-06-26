# Chezzi — Bug Discovery Strategy

> **Status:** strategy doc. Captures *how* we systematically find correctness bugs in the
> implementation before committing to large work (e.g. the Cranelift JIT). Most of the automated
> techniques below are **not yet built** — this is the ranked plan. Live status in
> [`PROGRESS.md`](../PROGRESS.md); design rationale for the language in [`spec.md`](spec.md).

## Why this exists

Manual edge-case probing (hand-writing `1.0/0.0`, `sum()` overflow, etc.) finds a handful of bugs but
does not scale and is biased toward bugs you can already imagine. Mature language implementations
**automate** bug discovery with adversarial input generators + independent oracles. This doc records
the techniques, who uses them, and the ranked plan for Chezzi.

## The blind spot in our current setup

Chezzi already has a strong **VM↔interp parity oracle**: every golden test asserts the bytecode VM and
the tree-walk interpreter produce byte-identical output. This is powerful — but it has one structural
blind spot:

> **The two engines were co-developed, so they share bugs.** Differential testing catches
> *divergence*, never *shared wrongness*.

Every bug found in the June 2026 hunt was invisible to parity because **both** engines had it:
- `List.sum()` silently wrapped i64 overflow on both engines → parity green → undetected.
- `nan < 1.0` faulted with a misleading message on both engines → parity green → undetected.
- Float div-by-zero faulted (non-IEEE) on both engines → parity green → undetected.

The remedy is twofold and is the core of this strategy:
1. an **external oracle** (compare against CPython — Chezzi is Python-feel) to catch *semantic* bugs both engines agree on, and
2. **fuzzing the Rust host** to catch *crashes* (panics on malformed input) the curated test corpus never exercises.

## What we already have

- **VM↔interp parity oracle** — `assert_parity` / golden `examples/*.chz` + `.expected` (~2736 unit tests). Catches engine divergence.
- **Grammar conformance** — `docs/grammar.bnf` is executed and differential-tested against the parser (`src/conformance.rs`, `tests/corpus/`, `cargo test conformance`). Syntax-level only — not semantics.
- **CPython bench harness** — `benches/run.chz` runs paired programs in `benches/chz/` (Chezzi) and `benches/py/` (Python) and compares **timing**. The paired programs are an existing seed for an **output**-differential oracle (see #2 below) — the infrastructure is half-built already.
- **Adversarial review pipeline** — `auto-task` (prosecute→defend→judge) + `post-merge-gate`. Good at vetting a *known* change; not a *discovery* tool.

## How real implementations find bugs

| Technique | Who uses it | What it catches | Fit for Chezzi |
|---|---|---|---|
| **Coverage-guided fuzzing** (libFuzzer/AFL) | Rust `cargo fuzz`; SQLite `dbsqlfuzz`; JS `Fuzzilli` | parser/checker **panics** on malformed input (should be clean errors, not Rust crashes) | ⭐ highest mechanical ROI — Rust-native |
| **Differential vs reference impl** | GCC/LLVM (CSmith vs `-O0`/`-O3`); JS engines cross-tested | **shared** semantic bugs — wrong output both engines agree on | ⭐ Python-feel + CPython harness already exists |
| **Metamorphic / EMI** | GCC/LLVM (Equivalence-Modulo-Inputs, 400+ bugs) | optimizer bugs: behavior-preserving transforms that change output | peephole/const-fold on==off, `x`→`(x)`, dead-code inject |
| **Property-based** (proptest / Hypothesis) | CPython, Rust | invariant violations: `parse∘print==id`, idempotence, roundtrips | compiler pipeline |
| **Miri / sanitizers** (UBSan/ASan/TSan) | Rust (Miri); Go (race detector) | UB in `unsafe`; data races | ⭐ GC + FFI + OS-thread engine are fragile (see the FFI SIGSEGV history) |
| **loom** (exhaustive interleavings) | Rust concurrency libs | concurrency races by exploring all schedules | channels / `Shared` / executor / netpoller |
| **Coverage measurement** | SQLite (100% MC/DC) | untested branches, especially error paths | `cargo-llvm-cov` |
| **Dogfooding real programs** | every language | ergonomic + integration + missing-feature + semantic bugs fuzzing can't reach | a real CLI app |

SQLite is the extreme reference point: ~600× more test code than source, every `malloc`/IO failure
path exercised via fault injection. Rust's **crater** runs a candidate change against all of
crates.io. The common thread is *automated oracles + adversarial input generation*, not hand-written
cases.

## Ranked plan for Chezzi

**Tier 1 — would have auto-caught the June 2026 bugs; closes the blind spot:**

1. **`cargo-fuzz` on lexer → parser → checker.** Feed random bytes/strings. Invariant: *the pipeline
   never panics — malformed input yields a clean diagnostic, never a Rust panic / `unwrap` / index
   out-of-bounds / arithmetic overflow / OOM.* A day of setup; typically finds bugs in batches. The
   single highest-yield mechanical lever for a Rust-hosted language.
2. **Differential vs CPython.** Build a corpus of programs valid in both languages; run both; diff
   stdout. This is the external oracle that defeats the shared-bug blind spot — `sum()` overflow and
   `nan <` would have surfaced immediately. Reuse `benches/chz` + `benches/py` and the `benches/run.chz`
   harness; extend it from timing comparison to **output equality**. Mind the documented intentional
   divergences (int overflow faults vs Python bignum; `/`,`%` truncation; float formatting) — encode
   those as an allow-list so they don't produce false positives.
3. **Miri + ASan on the `unsafe` surface** (GC, FFI/libffi, raw pointers, any `transmute`).
   `cargo +nightly miri test` for UB; ASan on the C-ABI paths. Targeted at the most memory-fragile code.

**Tier 2 — structural, medium effort:**

4. **`proptest` properties:** `parse∘print == id`; peephole on == off; const-fold result == unfolded
   result; `--serial` == default == interp over *randomly generated* programs (extends parity beyond
   the curated corpus).
5. **Generative grammar fuzzer** from `grammar.bnf` → random *valid* programs → run both engines +
   CPython. Complements #1 (which targets the error paths) by targeting the *accept* paths.
6. **TSan / loom on the concurrency engine** — channels, `Shared`, `Executor`, netpoller under race
   detection / exhaustive interleaving.

**Tier 3:**

7. **Coverage measurement** (`cargo-llvm-cov`) — find untested branches, especially error paths; write
   tests for the gaps. (The auto-task found bugs in exactly the spots that lacked adversarial tests;
   coverage makes those gaps visible.)
8. **Dogfooding** — write real programs (a JSON CLI, a mini-interpreter, a log analyzer). Finds
   semantic/ergonomic/integration bugs and missing-feature friction that fuzzing structurally cannot.

## Recommended starting point

Begin with **#1 (cargo-fuzz parser) + #2 (CPython differential)** — together they cover both *crashes*
(fuzz) and *wrong answers* (differential), and they are the two techniques that directly close the
parity blind spot. Run them unattended; triage findings through the existing
`auto-task` → `post-merge-gate` pipeline.

Pre-JIT gate: the JIT is a large, late-stage endeavor that assumes the interpreter semantics are
correct (it must produce byte-identical results to the VM). Standing up Tier 1 first — so the
reference semantics are fuzzed and differentially validated — de-risks the JIT before a line of it is
written.
