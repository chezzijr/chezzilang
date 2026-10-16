# Bug-discovery records (moved verbatim from `docs/contributing/bug-discovery.md`, 2026-10-16)

## The blind spot in our current setup

**Historical framing (2026-06/07), kept because the lesson outlived the mechanism.** Chezzi then had a
**two-VM parity oracle**: every golden test asserted the cooperative `--serial` VM and the default M:N
VM produced byte-identical output. That was a real differential for *concurrent* programs — but it had
one structural blind spot:

> **Both engines were the same `Vm`; for sequential programs they ran identical bytecode.** So for
> non-concurrent code, parity was a determinism check, not a differential — it caught
> *divergence*, never *shared wrongness*.

**`--serial` was removed 2026-08-16** (`docs/design/roadmap.md` §2b), so that oracle no longer exists at all;
what stands in its place for accidental *schedule*-dependent divergence is the two-worker-count gate
(Tier 2 #4 below). The blind spot argument is unchanged by the removal — it is the reason the CPython
differential and the DSA known-answer harness were built, and both are live.

Every bug found in the June 2026 hunt was invisible to parity because **both** engines had it:
- `List.sum()` silently wrapped i64 overflow on both engines → parity green → undetected.
- `nan < 1.0` faulted with a misleading message on both engines → parity green → undetected.
- Float div-by-zero faulted (non-IEEE) on both engines → parity green → undetected.

The remedy is twofold and is the core of this strategy:
1. an **external oracle** (compare against CPython — Chezzi is Python-feel) to catch *semantic* bugs both engines agree on, and
2. **fuzzing the Rust host** to catch *crashes* (panics on malformed input) the curated test corpus never exercises.

## Panic-fuzz: the depth axes

**Grammar-structure blind spot (deeply-nested constructs).** The three generators are byte/token-level:
they produce random bytes, sample the token alphabet, and mutate the corpus — none *recursively deepen a
specific grammar production*. So a crash that needs, say, ~10 000 levels of one nested construct is
statistically unreachable (the token sampler would have to emit thousands of the *same* opening token
consecutively by chance). The **2026-07-12 manual hunt found exactly such a bug the fuzzer missed**: a
deeply-nested `match` **pattern** (`?(?(…?0…))` / nested tuples) overflowed the host stack
(SIGABRT, exit 134) because `parse_pattern_impl` was the one recursive-descent entry point with no
`MAX_DEPTH` guard (fixed — `dcde045`). Lesson: crash-safety over *depth* per grammar production is a
distinct axis from the byte/token fuzzing here — either add a grammar-aware generator that recursively
nests each production to `MAX_DEPTH+N`, or (cheaper) assert every recursive-descent parse fn shares the
one depth guard. Until then, "zero fuzzer crashes" does not certify the deep-nesting axis.

**Now partly asserted (W7-50, 2026-08-13).** `parser::tests::max_depth_boundary_accepts_then_rejects`
is the per-production depth assertion this section asks for: it **bisects** the accepted nesting count
for each of the seven guarded productions (parens, lists, calls, blocks, unary, generic types, match
patterns), asserts `k` parses / `k+1` rejects with `"too deeply"` (never a host abort), and asserts
`k >= 200` — CPython 3.14's own limit — for the four directly comparable forms. The boundary is
measured, never hard-coded, so a widening AST node shows up as a *number in a failure message* instead
of as a silent shrink. Its end-to-end sibling
`check_errors_json::worst_accepted_nesting_never_signal_crashes` drives `ast`/`check`/`run` over the
worst program the parser accepts. What is still missing is a *generator* that reaches these shapes from
the fuzzer, so the corpus axis remains uncertified.

**Post-parse walker depth axis (iterative chains) — found 2026-07-12, fixed `fix/frontend-deep-stack`.**
The `dcde045` guard closed deep *recursively-parsed* constructs (patterns, parens, types). But the
`MAX_DEPTH` guards only bound the parser's *recursion* — and left-associative binary chains (`1+1+…`)
and postfix chains (`x.f.f…`, `a[0][0]…`, `f().g()…`) parse in the **iterative** `while`/`loop` bodies
of `parse_bp` / `parse_postfix`, which never bump `self.depth`. So the parser *accepted* a valid but
thousands-deep left-leaning AST that the **post-parse recursive walkers** — `desugar::walk_expr`, the
checker's inference walk, the compiler's lowering — then overflowed on (SIGABRT, exit 134, on the CLI
main thread *and* the ~2 MiB LSP worker; uncatchable by `recover:` — it is a *compile-time* host abort
on a well-typed program, strictly worse than the malformed-input pattern case). Two-part fix: (1) a
`MAX_CHAIN_DEPTH` per-loop cap in both iterative loops bounds a single chain's length (so accepted AST
depth is bounded by `MAX_DEPTH` × `MAX_CHAIN_DEPTH` — **superseded by W7-50**, see below); (2) the whole front-end (resolve → desugar →
check, and `run`'s build+compile) runs on a dedicated large stack — `chezzi::on_frontend_stack` /
`FRONTEND_STACK_BYTES`, mirroring the VM's `VM_STACK_BYTES` thread — so that bounded depth is walked
with headroom regardless of the caller's stack size. A parser cap **alone** cannot fix it: any cap high
enough for real code (100+-term chains) still overflows the 2 MiB LSP walker stack, so the large stack
is load-bearing (this is the `recursion-guard-smallest-stack` rule — a bare depth cap must be sized for
the *smallest* stack the guarded recursion runs on). Lesson: crash-safety has a **post-parse walker**
depth axis distinct from parse-time recursion; a recursive AST walker must run on a stack sized for the
worst parser-accepted depth (or share the depth guard), not just the parser.

**Superseding correction (W7-50, 2026-08-13).** The `MAX_DEPTH` × `MAX_CHAIN_DEPTH` product was a
*bound*, not a *measurement*, and it stopped holding the moment `MAX_DEPTH` was raised from 64 to 512
to reach CPython parity (512 × 500 = 256 k nodes, against a measured ~33 k-node cliff). The per-loop
proxy is gone; `parser::MAX_AST_DEPTH` (16 000) now bounds the resource directly — the *synthesized*
fold depth of the tree being built (`Parser::fold_depth`), so it composes across nesting levels the way
the tree does. An **ambient** counter decremented on loop exit was tried and rejected with evidence: a
fold loop runs AFTER the descent it sits above, so in `f(g(DEEP) + 1 + 1 + …)` the folds and the descent
never coexist in an ambient counter — measured, that shape parses at 45 000 nodes deep and SIGABRTs
`chezzi run`. Same lesson as W7-55's `Eq`-walk node budget: bound the resource that is spent, and prove
the bound against the shape that composes, not the shape you had in mind.

**And the axis NARROWED, deliberately — say so, because the first writeup said the opposite.** W7-50
originally shipped claiming 16 000 was above everything that parsed before, so nothing regressed. It
was not: a paren level can spend BOTH fold loops (`parse_postfix`'s and `parse_bp`'s) at ~998 nodes per
level, and `x := ` + *k* × `( … .f×499 +1×499 )` bisects to *k* = **30** (~29 940 nodes) on `b1307258`
versus *k* = **16** (~15 968) today. Against the ~33 100-node cliff that is **1.11× → 2.07×** headroom:
the old ceiling was accepting programs within ~10% of an uncatchable abort. Two lessons for this
document's own method. (1) **A limit's "no regression" claim is a measurement, not a corollary** — it
must be bisected on the PRE-change binary against the *worst* shape, and the worst shape is the one
that spends every loop a level can spend, not the first one that came to mind. (2) When a hunt's fix
narrows what is accepted, that is often the *win*; hiding it behind a false no-regression claim makes a
good change unauditable (same family as `rule fires ≠ rule is right`).

**Third correction (same gap, follow-up): a per-*instance* limit is not a limit on the axis.**
`MAX_AST_DEPTH` was enforced by the `Parser` object that builds a tree — and an interpolated `{…}`
fragment is re-lexed and built by a **second** `Parser` whose counters start at zero. So the axis had
no bound at all: *L* nested levels of `"{ <15 985 deep> }".len()` bought *L* × 16 000, measured at
peak walk depth 15 000 / 30 000 / 45 000 for L = 1/2/3, and debug `chezzi run` SIGABRTed at L = 3 on a
program `chezzi check` accepted. The generalizable lesson for this document: **when a bound lives on
an object, ask what happens when the pipeline makes a second one.** Grep for every constructor of the
bound-carrying type (`Parser::new` here) — a re-parse, a re-lex, a spliced fragment or a macro
expansion is a fresh instance, and a resource bound that resets per instance is a bound on nothing.
The repair that composes is to enforce on the **finished artifact** at a seam every consumer routes
through — here `desugar::Walker::walk_expr`, whose recursion depth is the composed tree's depth —
rather than to thread a remaining budget through each re-entry, which is only as good as the callers
that remember to pass it.

**And the second half of that lesson, which the review had to supply: "finished" is a claim to check,
not a word to use.** The chosen seam is finished for everything the walk *reaches*, and `desugar`
then spliced default arguments in the **tail** of the walk, after that node's children
had already been visited — so a splice performed on the second (last) pass was never walked, and a
well-formed interpolated literal inside it still reached the checker and compiler un-converted, at
~2× the bound. When you enforce on an artifact, enumerate every writer that can still mutate it after
your check, not just every reader that consumes it. A tail-position mutation in the same walk is the
easiest one to miss, because it does not look like a second pipeline stage.

**Closed by W7-51, and the way it was closed is the third half of the lesson: delete the writer.**
That residual needed two things — a second pass, and a spliced default *expression*. W7-51 removed
both (one pass; a non-literal default is compiled once in its defining module and the call site gets
a two-node call to it), so the class is gone by construction rather than by a third guard. **Verify a
"gone by construction" claim with a probe on the mechanism, plus a negative control**: a probe on
`checker::check_interpolation`'s success arm fires exactly when a well-formed `Str` reaches the
checker un-converted, and on the fixture the residual was recorded with it read **1 hit on
`925dd0f7`, 0 on `ed4830b3`** — with **2 hits** on the same `ed4830b3` build once the desugar
`Str → Interp` conversion was disabled, which is what makes the zero evidence instead of an absence.
A zero from an unfalsified probe proves nothing about the code and everything about the probe.

Status: the `0..2000` gate is green, and unattended sweeps of `0..100000` (release, overflow-checks
OFF) and `0..20000` (debug, overflow-checks ON) found **zero** panics or signal crashes — the
front-end is crash-safe over the *byte/token* inputs explored so far (see the deep-nesting blind spot
above: the deep-pattern SIGABRT was found by manual audit, not the fuzzer, and is now guarded).

## Differential oracle: float and shim notes

- **Floats** (`floats`, on in `full()` since `docs/contributing/gaps.md` §W7-37): float **arithmetic**, not just
  float literals. `gen_float` is recursive like `gen_int` — `Add`/`Sub`/`Mul` over in-scope float
  vars, `try_call`/`try_index` leaves, and literals — and floats are assignable (`=`/`+=`/`-=`).
  Leaves are `n/8` scaled by `2^e`: exact (a power of two moves only the exponent field, so the
  "both engines compute the identical double" argument holds), and the scale is what actually
  reaches Python's scientific-notation crossover (`|x| >= 1e16` / `< 1e-4`) — `n/8` alone tops out
  near `1e8`. That crossover is where shortest-`repr` formatting is most delicate and where `W7-32`
  lived. The per-leaf `[-70, 60]` scale bounds a single LITERAL leaf, not the generator overall —
  a float var or a `try_call` result can carry a prior multi-leaf product, so magnitudes compose
  across statements and functions and `inf`/`NaN`/`-0.0` are structurally reachable; measured 0
  `inf`/`NaN` (25 `-0.0`) across 5000 `Features::full()` programs, and both engines render all
  four byte-identically when they do occur. **Comparison** (`< <= > >= == !=`) landed 2026-08-07:
  `gen_bool`'s comparison arm now sometimes draws both operands from `gen_float` — premise
  verified first, 4005 cases byte-identical against CPython. **`Div`** landed the same day:
  `gen_float` emits it with no non-zero-divisor guard (float `/` is total IEEE-754,
  `docs/reference/spec.md:472` — a zero divisor is `inf`/`-inf`/`NaN`, never a fault), and the Python shim's
  `_chz_fdiv` absorbs Chezzi's deliberate divergence from CPython's `ZeroDivisionError`.
  **Int↔float mixing** (`1 + 2.0`) landed 2026-08-07 too — the filed reason it was deferred
  ("needs a coercion model in the IR") was wrong: `Bin`'s `ty` is already the result type, so a
  mixed node renders correctly through both emitters with no IR change. `gen_float`'s composite
  arm and `gen_bool`'s float-comparison arm each mix in an int operand (0.3 chance), side
  randomized so both `int op float` and `float op int` are generated; `MAX_BOUND` now straddles
  `2^53` (`W8-23`, TICKET-014) and the VM compares an int against a float exactly, so mixed
  comparison stays correct without capping the bound. **`Mod`** landed 2026-08-08, the last unreached float
  operator: `gen_float` emits it beside `Div`, same no-non-zero-divisor discipline, and rides the
  same `op` variable the mixing arm already uses so mixed int↔float `%` comes for free. Chezzi's
  float `%` is `fmod` — sign of the DIVIDEND, matching Rust/Go, diverging deliberately from
  CPython's floored, raising `%` — and the Python shim's `_chz_fmod` absorbs both the sign rule and
  the totality (a zero divisor or an `inf` dividend is `NaN`, where `math.fmod` raises on both).
  §W7-37 is now fully closed.

**Every scalar type is reachable through a call and an index.** `try_call`/`try_index` used to be
asked only for `Ty::Int`, so ~2/3 of generated functions were emitted and never invoked and non-int
element reads never happened — code both engines "agreed" on because neither ran it. `gen_bool`,
`gen_str` and `gen_float` now each attempt both (§W7-37). `try_call`'s int-argument discipline is
unchanged: an int arg stays a small literal, because the callee's body and `ret_bound` were
generated assuming `|int param| <= PARAM_BOUND`.

The i64-no-overflow guarantee is preserved across these: the only new path where an int value crosses a
seam is a tuple-field read, which inherits the element's tracked `tuple_bounds` and is never emitted
inside an in-loop accumulator RHS; method (`str`/`bool`/`List[str]`), `in` (`bool`), and slice
(collection/`str`) results carry no int value, so they add no seam.

**One shim arm became the identity (2026-08-06).** A string nested in a container used to render
raw, so `_chz_repr` returned the string unchanged; it is now CPython `repr` (`docs/contributing/gaps.md` §W7-25),
so that arm is literally `repr` and the oracle proves the nested-string rendering equal rather than
absorbing a difference. Note the direction of the hazard: while the divergence existed, the shim
ENCODED it, so the oracle could never have reported it — a detector written to mirror the
implementation cannot see a bug in what it mirrors.

## Seeded scheduler oracle: the harness false positive

**Fixed: a harness false positive that used to cry wolf on a clean tree, twice over.** `corpus()`
includes any `examples/*.chz` with a sibling `.expected` and a concurrency keyword. Three problems
compounded: (1) `measure_baseline` sampled only ONE unseeded run at T=1 and ONE at T=2 — REGARDLESS
of the worker count the job it judged actually used — so a job run at `CHEZZI_THREADS=0` (the
default) was judged against a baseline that never ran at that count at all, and a flaky exit code
could read as stable on luck alone; (2) even a correctly-measured unseeded baseline cannot bound
byte-exactness for a program that SPAWNS a separate printing task, because `chezzi run`'s cross-task
print order is nondeterministic BY CONTRACT (`docs/reference/concurrency.md` "Output ordering: streaming CLI
vs buffered sink") — a first fix read this off a `spawn`/`Executor` keyword and silenced the output
check on 42 of 46 examples (`contains("spawn")` also matched comments), which is worse than the false
findings it hid; (3) the sweep's OWN concurrent job load creates enough CPU contention to make a
wall-clock-ratio-gated Chezzi test flake, unrelated to scheduling at all.

Landed fix (`src/schedfuzz/mod.rs`): `measure_baseline` now takes a `threads` argument and runs
`BASELINE_REPS = 5` unseeded reps ALL AT THAT COUNT — the same one the job it judges will use — never
a hardcoded T=1/T=2 pair; the `(target, threads)` cache key already existed, it just wasn't being fed
the right baseline. `target_for` no longer reads any keyword — it always loads a sibling `.expected`,
and whether the output check is actually trusted is decided purely by SAMPLING: a stable exit code
with disagreeing stdout across the `BASELINE_REPS` turns off `check_output` without marking the whole
target `UNSTABLE`, so hang/panic/rc checks stay live. This sampled check does not bound
`parallel_cross_nursery_ok.chz`-class divergence (measured 0/40 unseeded `CHEZZI_THREADS=2`, yet a
real race) — a `KNOWN_TARGETS` list (`src/schedfuzz/mod.rs`) now skips such targets BY FILE NAME on a
default (no `--program`) sweep, citing the `docs/contributing/gaps.md` row that explains each: **W15-4** for six
programs whose printing crosses a `spawn`/`Executor` boundary (the documented contract itself, not a
bug), **W15-5** for two wall-clock-ratio-gated `tests/chz` files that flake under the sweep's own CPU
contention (measured: `regex_test.chz` 1/10 unseeded under SUSTAINED sibling load, 0/30 with none),
and one entry each for the still-open **W15-3** (net), **W15-6** (a generator-over-channel hang) and
**W15-7** (a cancel-propagation hang) so a routine sweep does not keep re-reporting an already-filed
bug as new. `--program <path>` bypasses the skip list — it always runs the target you name. A
`KNOWN_TARGETS` entry currently skips the WHOLE target, so it also drops that target's hang, panic and
rc checks, not just the output check its reason names; an output-only skip (leaving the other three
live) is the upgrade path, not yet built.

Measured 2026-09-23, `--seeds 1..17 --threads 1,2,0`, four full corpus sweeps across the two fixes:
the first (worker-count-blind baseline, keyword-gated `.expected`) flagged `output` findings only on
`try_recv.chz`/`parallel.chz`/`parallel_cross_nursery_ok.chz`/`channel.chz`/`channel_block.chz`,
`rc`/`hang` findings on `regex_test.chz`/`generator_channel_test.chz`, and printed
`nested_nursery_open_outer_body_test.chz` as `UNSTABLE` (a SKIP, not a finding — its old T=1/T=2-mixed
baseline sample happened to disagree). After the worker-count fix, the SAME `nested_nursery_open_outer_body_test.chz`
row is no longer a lucky `UNSTABLE` skip: judged at its own swept `T=2`, its baseline is stable and its
wall-clock-ratio flake now scores as a real `rc` finding, joining `regex_test.chz` in **W15-5**. After
both fixes and the `KNOWN_TARGETS` skip list, a clean-tree sweep reports 0 unexplained findings and
exits 0:

```
done: 177 targets, 8496 runs, 0 finding(s), 0 unstable, 11 known
```

every remaining divergence is either the documented contract (**W15-4**), a
load-sensitive test-infra flake (**W15-5**), or an already-filed bug (**W15-3**, **W15-6**, **W15-7**).

## Load hygiene: the W15-8 incident

Never start one as a bare background `while` loop. `pkill -9 -f "<pattern>"` kills the PROCESSES the
loop spawns, not the loop itself — the loop just respawns them, and every later measurement on the
box (this ticket's own included) is then silently taken under load nobody intended. This is what
produced **W15-8**: 28 orphaned `while true; do CHEZZI_THREADS=2 timeout 3 chezzi test
tests/chz/stdlib/regex_test.chz; done` loops plus 4 `yes` processes, left over from an earlier load
experiment whose `pkill` had only killed the spawned test processes, held `uptime` load at 33-34 for
hours and made an unrelated test misread as a flaky regression. `systemd-run --scope` ties the whole
tree to one cgroup that `systemctl stop` tears down in one shot, loops included.
