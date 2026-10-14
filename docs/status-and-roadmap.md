# Status and roadmap

Moved out of `CLAUDE.md` (which every agent turn reads). `PROGRESS.md` is still the single source of truth for what is next.

## Where things stand

Core language is **implemented through M24 (and still evolving; M19 perf in progress)** (scalars, `List`/`Map`/`Set`/`tuple`,
generic structs + enums, `T!E`/`T?` + `?`, generics + structural protocols,
exhaustive `match` + guards, closures/HOF, modules (incl. Python full paths `a.b.X`, TICKET-175),
GC, interpolation, pipe, `defer`, `recover:`, `Iterator[T]`, slicing/indexing protocols,
user-overloadable `==` via `Eq`, static protocol requirements via witness passing). **Concurrency** has
landed through **Tier-D** (`spawn` / `parallel:` nursery, `Channel[T]`, `Shared[T]`, `Executor`, the real
OS-thread M:N engine, netpoller + `std.net`). The checker has a **non-fatal warning channel**
(`Severity::Warning`, `"severity"` in `--errors=json`, `DiagnosticSeverity::WARNING` in the LSP) with
five rules — a discarded `T!E`/`T?`, a mutating call or a store on a temporary copy (a
`Shared`/`RwShared`/`Atomic` read, a builtin slice, `.copy()`, an operator result, TICKET-254), an unreachable `match` arm, an unread local (TICKET-090), a near-miss hook that its
operator skips (TICKET-250). Test totals move with every ticket;
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
> a `docs/gaps.md:NNNN` citation in an old ticket resolves against the archive. **9 open rows** today —
> W8-19, W13-28, W15-4, W15-5, W15-8, W15-10, W17-1, W22-1, W23-1, all P2/P3 or tracking rows (this count is
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
