# Chezzi — Engineering Lessons

> **Status:** hard-won rules, distilled from the project's working memory (2026-06 → 2026-09) so they
> are visible to everyone, not just one person's notes. Each entry is a rule, the measured incident
> that produced it, and where to look. Newest ledger detail lives in [`gaps.md`](gaps.md); the hunt
> strategy in [`bug-discovery.md`](bug-discovery.md). **Read the section for the area you are about
> to touch before you touch it** — most of these shipped fully green the first time.

---

## 1. Design north star

**Chezzi is a magpie language. Drift from the owning ancestor is a bug, not a design choice.**

| surface | ancestor | examples |
|---|---|---|
| syntax, scripting feel, `Executor` ergonomics | Python | indentation, interpolation, slicing, truthiness |
| interfaces + concurrency | Go | structural `protocol`s, `spawn`/`parallel:` nursery, `Channel`, cancellation points, `defer` |
| types, errors, control flow | Rust | `enum` + exhaustive `match`, `Result`/`Option` + `?`, `panic`/`recover` |

- **Correctness outranks engine agreement.** There is one engine now, so "both engines agree" is not
  even available as a defense. The defense is the ancestor's *measured* output: write the Go/Python/
  Rust program and run it. A semantics that surprises a Python/Go/Rust user needs a reason a *user*
  would accept, never a reason the test suite would.
- **When a heuristic verdict is unsure, it must decline** (hang, stay silent, ask) — never emit a
  confident wrong answer. A missing answer is recoverable; a wrong one teaches distrust of every answer.
  Ranking: correct > no answer > wrong answer. Worked example: `gaps.md` **W7-12**.
- **Fake determinism is drift.** Output was once buffered per task and flushed in task order so two
  engines could match byte-for-byte; that cost users interactive prompts and any output from a hung
  program. Keep determinism in the *test sink* (order-insensitive assertions), not in the language.
- **Fully resolve the issue; do not file a residual as a substitute for fixing.** Three consecutive
  fixes each closed one row and opened one or two. A filed residual reads as progress but is a transfer
  of work, and its premise decays (see §7). Filing is right only for something genuinely out of scope.
- **Stdlib gaps are deferred on verified cost, never on "nobody asked."** A reference-language idiom
  *is* the need. `Reader.lines()` was deferred on a claimed cost ("needs a new lazy Obj variant") that
  was false — a generator over `read_line()` streams lazily by construction — and building it surfaced
  two real check-OK/run-broken holes. Spike before trusting an old "too expensive" note.

## 2. Verification: what a green gate cannot see

**The standing gates are blind to whole classes.** A silent wrong answer has no assertion to fail; no
gate measures performance; no gate reads a message; no gate executes prose.

- **A change that widens what is rejected (or narrows when an error is returned) is structurally
  untested by the suite that passed before it.** The suite is a sample of programs that were legal
  before; the change's blast radius is by construction outside that sample. Four batches of this
  shipped fully green (4000+ tests, clippy) and were caught only by adversarial review running the
  **pre-change binary** on neighbours nobody had written down. Before landing any reject/refuse/error
  predicate: write the premise, enumerate the neighbours it implies (same shape, different type /
  context / worker count), and run them on the pre-change binary in a separate `CARGO_TARGET_DIR`.
- **"The rule fires" is not "the rule is right."** A `rejects()` test passes identically whether the
  reason is true or invented. `fn neg` on a numeric newtype was banned as "operator-named" — but unary
  `-` has no newtype path at all, so the rule deleted the only spelling of negation while asserting a
  conflict that cannot occur. For each case in a reject set, prove the harm exists *for that case* on
  the pre-fix binary; derive `ok()` neighbours from the premise, not the name category.
- **Pick the test shape that can expose the mistake, not the easiest one.** A carve-out was tested on
  `map` (slot `fn(E) -> U`, no concrete parameter) and was broken on `fold` (slot `fn(A, E) -> A`,
  where the concrete `A` reveals it). Ask which variant has the feature that would show a defect.
- **A symmetric test for an asymmetric relation is a bug regardless of test results.** One W7-42
  guard was cut wrong three times, each fully green; the third produced a silent wrong value. If
  swapping the operands changes the question, the predicate must be directional. Derive neighbours by
  *order* (swap two statements, insert an unused one) and prefer the existing directional helper
  (`merge_unknown` already answered it).
- **A checker warning's gate is derived from the runtime, one program per shape.** Both rules on the
  warning channel got their gate wrong on the first attempt by trusting a plausible checker flag.
  Enumerate every position the rule could fire in and *run* each; when unsure, under-warn.
- **Installing a bound means grepping the structural shape for every site, not enumerating from
  memory.** W7-50 moved the parser's depth bound and counted two fold loops; a third
  (`parse_type_postfix`) SIGABRTed on both binaries. A per-production sizing oracle is written from the
  same list as the fix, so a missed site is missed twice and goes green.
- **A lossy decode blinds a comparison oracle.** `from_utf8_lossy` maps every invalid byte to `U+FFFD`,
  so a comparator that decodes before diffing passes byte-divergent runs. When a change widens what a
  program can *emit*, audit the detectors in the same commit. Falsifiability test: feed `ff fe` vs
  `fe ff` and assert the comparator panics.
- **Verify list contents via `.len()`/index, never from `print()`.** `"".split(",")` was filed as a bug
  because `[""]` renders as `[]`. An unfamiliar behavior is not a bug until an oracle says so — the
  from-imported-global "stale copy" was another near-miss: CPython prints the identical `1 / 0 / 1`.
- **Adversarial review is a required step, not a contingency.** Every incident above passed the full
  gate and was found by a prosecutor building both revisions. Budget it for anything touching checker
  soundness, runtime verdicts, the airlock, or an error-vs-proceed decision. See the `adversarial-review`
  skill and `bug-discovery.md`.

## 3. Checker soundness

**The compiler is type-blind by construction** (`compile_graph` receives only the AST). Therefore the
checker's accepted set must be a **subset** of what the compiler can lower. Every violation is a
check-OK-then-broken program, and a JIT would trust the static type — this class must be swept before
the freeze.

- **Fix by narrowing the checker, never by plumbing types into the compiler.** Where the two must
  agree on a syntactic predicate, put one shared predicate in `src/ast/mod.rs` and call it from both
  (`const_num`, the int→float rule) so they agree by construction. Six of thirteen wave-5 bugs were
  this one shape: int-under-float (`.sort()` returned an unsorted `List[float]`), range-as-value,
  bound-method-as-value with `self: Unknown`, `return` in `defer:`/`spawn:` silently discarded, nested
  `import` as a no-op.
- **Int→float widening is the Go model.** Untyped *constant* expressions adapt (`x: float = 1 + 2` →
  `3.0`); a typed int *value* never converts (write `float(x)`). Do **not** join `int`/`float` in an
  inferred return — the compiler reads the annotation, not the inferred type, so no coercion is
  emitted and `x / 2` does integer division under a `float` type. Always runtime-verify an inferred
  float; `check` alone cannot see it.
- **An un-inferable `Unknown` in an inferred return is a type-check bypass** (`compatible(Unknown, _)`
  is true). `fill_ret` must be exhaustive over every `Ty` variant that carries an inner type — no
  catch-all — so a future variant fails to compile instead of re-opening the leak. The first cut's
  `other => other.clone()` missed `Shared`/`Channel`/`Atomic`/`Func`.
- **Defaulting a slot to a protocol without checking satisfaction launders.** The inferred `Result`
  E-slot defaults to `Error` only if `Unknown` or the payload *satisfies* `Error`; the unconditional
  first cut let `e.message()` type-check on a payload with no `message`, then fault at runtime.
- **A hoisted declaration's guard must be symmetric in source position.** A top-level `fn` is defined
  into its slot before any statement runs, so `fn_span < let_span` tests a quantity the runtime does
  not have; one statement swap defeats it. Gate on *readers* (was anything above already typed against
  the fn?). `Ty::Func` equality ignores optional arity, so a stricter re-binding needs its own
  directional check.
- **A guard keyed on a module-wide bare-name table fires on a parameter or local that merely shadows
  the name.** `fn f(pi: float)` + a later `import pi from std.math` reported "`pi` is used before its
  import" — factually false. Require scope-0 resolution (`scopes.iter().skip(1).any(...)` means
  shadowed). A 435-file sweep showed zero changes and still missed it.
- **A checker→compiler side table keyed on a `Span` aliases wherever one coordinate is reachable under
  two identities.** Four instances: a re-lexed interpolation fragment, a default-param expression
  spliced across modules, chained `a?.b?.c` links, and a desugar-*synthesized* variadic pack carrying
  the call's span (`[1, 3.0] |> vari(2.5, 1)` printed `[[1, 3.0], …]` instead of `[[1.0, 3.0], …]`).
  Fix by making the coordinate real (`Span` grew `file`; the pack grew an origin field), never by
  re-anchoring a span. `Span` is 12 bytes and sizes `MAX_DEPTH`; growing it is a user-visible
  nesting regression.
- **A synthesized declaration is a real declaration.** W7-51's hidden default-provider fns had to be
  (1) injective over the full owner key, kind included (`struct S` and `fn S` collided); (2) legal in
  the position they land in (a free fn cannot spell `Self`); (3) reachable by a resolution path that
  carries the module coordinate — name-keyed method lookup carries none, and no guard invents it.
- **Generic operator overloads need the type's own substitution *and* `compatible(l, r)`.** Bind the
  receiver's params on the actual side only, then require matching type args — name-only equality
  let `Box[int] + Box[str]` infer `Box[int]` over a `Box[str]` value. Always add the heterogeneous
  boundary test on both struct and enum.
- **Always test an operator protocol through a generic bound** (`fn has[C: Contains[int]](c): n in c`).
  Concrete-type tests are structurally blind to the missing `Ty::Param` arm; `<` through `Comparable`
  is the analog to mirror.
- **`where T: <scalar>` is an equality constraint**, and a harvested `where` is enforced only where
  the receiver's method-call arm calls `enforce_bounds` — `Ty::Channel` did not until `trip()` leaked
  `bool` through `Channel[int].recv()`.
- **Two test entry points, two checker paths.** `ok()`/`rejects()` check a single module with bare type
  keys; `entry_ok()` runs `build_graph` + `check_graph` with `<module>::Name` keys, which is what the CLI
  does. A test can be green on `ok()` while `chezzi check` rejects the same program. For anything
  touching module-scoped type resolution, use the graph path or both. The reserved-type method
  surface is likewise harvested on **two** paths (`mod.rs` graph harvest and `setup.rs` single-module
  harvest); a `native struct` mirror must seed both.
- **A native `Ty::Struct`-modeled type missing from the reserved/collision guard is accept-then-trap.**
  `import X from M` + `struct X` must be a clean "already defined" error. A struct-returning native
  threads through four fixed lists (`std/M.chz` decl, compiler layout array, `seed_stdlib_structs`,
  `types_by_name`), field order load-bearing.
- **`Eq` satisfaction is scalars-only, so any guard that asks `satisfies(_, "Eq")` over-rejects**
  (`Box([1,2]) == Box([1,2])` was legal and became an error). W7-41 stays open until `Eq` agrees with
  structural `==`; the reverted attempt's three traps are in the ledger row.
- **A parallel filtered `cargo test --lib <filter>` is not a trustworthy pass/fail signal** for checker
  graph tests — they share process-global state and collide when scheduled together. Gate on the full
  `--lib` run or add `--test-threads=1`.

## 4. Runtime, airlock, concurrency

- **A runtime verdict that declares user code broken must be built from what is *impossible*, never
  from what looks idle.** Three deadlock predicates in a row faulted healthy programs (a cap-1
  producer/consumer pipeline is permanently all-parked *by design*, 2–7 of 30 runs); the "obvious"
  progress-counter repair still fired 6/40, because on a polling runtime "nothing moved recently" is
  not "nothing can move". Restrict the verdict to the shape you can prove, decline everything else,
  fence it with a *looping* test, mutation-check that the test fails against the buggy predicate, and
  audit the party set for self-reference (a job joining its own `Executor` counted its own slot and
  was unsatisfiable by construction: 9/60 false deadlocks on debug, 0/40 on release). **A verdict that
  is right 85% of the time on the same program is not a verdict.**
- **The airlock has two layers, and only the runtime one is load-bearing for memory safety.** The
  checker's `assignable` sendable clause is an early nice error; `to_wire` + `has_handle` +
  `ensure_crossable` is the real net. But the net only covers sites that *call* the guard: value-store
  paths (`Channel.send`, `Shared`/`Atomic` set/update/CAS) once skipped it and an FFI handle crossed
  un-gated into a garbage cross-heap `GcRef` — genuine UB. Every new cross-heap store goes through
  `to_wire_crossable`, never bare `to_wire_at`. A missed *runtime* guard is UB; a missed checker
  widening is at worst an uglier error.
- **Closures cross by value iff every capture is sendable** (Rust `Send` model). A captured-local `ref`
  at a direct `spawn` is a compile error; one reaching the airlock indirectly is a loud runtime fault —
  and that fault may live **only** in the `Obj::Closure` capture arms of `to_wire`/`to_snap`. A
  module-global `ref` (`counter: ref int = 0`) is a read-only snapshot and must never be gated; the
  general-arm and reach-gate attempts both broke it.
- **Capture is free-variable, by reference.** A superset capture is safe; a subset silently breaks. The
  analyzer is exhaustive over every `ExprKind` with no `_` arm, and the same analyzer drives
  cell-boxing, so a gap would already crash existing closures.
- **Cycles identity-preserve through every container; only a generator does not.** A cycle re-entering
  a non-preserved node serializes "successfully" and *duplicates* it — silent aliasing break, caught by
  prosecutors twice. Guard: a generator still on the DFS stack rejects cleanly. A non-sendable
  module-global generator snapshots as inert `Nil` and faults only when reached; eager-faulting the
  snapshot regressed programs that merely held one.
- **A data-snapshot `Iterator[T]` crosses by deep copy like a `List`.** Do not "mirror the generator
  pattern" for a cursor — one line (`[1,2,3].iter()`) made a `to_snap` `unreachable!` reachable.
- **Container key/membership equality must fault like `==`, not swallow.** `values_equal` was
  `values_equal_guarded(..).unwrap_or(false)`, so `s.has(cyclic)` returned `false` where `a == b`
  faulted (CPython raises on both). Every production site now uses the guarded form via the
  `seq_slot`/`set_slot`/`map_slot` helpers and the swallowing wrapper is `#[cfg(test)]`-only, so a new
  miss is a compile error. Grep both `self.values_equal(` and `vm.values_equal(`.
- **Struct/enum/newtype map and set keys are snapshotted on insert** (Go value-key model); values stay
  by reference. A key fetched via `keys()` is the stored key by reference — decided WON'T-FIX, matching
  Python/Java.
- **Any `+1/-1` counter that gates scheduling must survive a Rust panic.** `Vm::guarded` brackets
  `native_reentry` around every native→Chezzi re-entry; once FFI callbacks made a panic recoverable,
  the plain decrement was skipped on unwind and every later blocking op demoted instead of parking.
  `catch_unwind`, restore, `resume_unwind`.
- **A libffi callback `Cif` must be `Box`ed** — the raw pointer libffi keeps dangled when the owning
  `Vec` reallocated. FFI UB is layout-dependent: all goldens passed while the CLI segfaulted
  deterministically. Verify FFI via the built binary. Segfault without gdb: a temporary SIGSEGV handler
  capturing `Backtrace::force_capture()` + `CARGO_PROFILE_RELEASE_DEBUG=line-tables-only`.
- **A compound mutate-and-return over `Shared`/`RwShared` does check + mutate + capture inside one
  `write` closure**, stashing the result in a captured box struct. Insert-then-separate-`read` lost the
  key to a concurrent `remove`.
- **`emit_out`/`emit_err` are no-ops on a dead pipe; the halt is re-raised at the call site.** A new
  native that emits to stdout must check `stream_halt(span)` after it returns, plus a `| head -1` CLI
  test — `Writer.write` once spun forever growing an unbounded queue.
- **A `NativeRet::Map` lowered from a `HashMap` carries randomized order; sort keys before lowering.**
  Any order-observable native value from an unordered Rust container must sort.
- **Native `Kind` is not paperwork.** `Blocking` offloads to the dirty pool; `Inline` pins a core
  worker. Getting it wrong is a live starvation bug (`future.md` §3c).
- **Recursion backstops are sized for the smallest *production* stack, then measured.** The LSP worker
  is ~2 MB (tokio default); a guard sized for 8 MB main still SIGABRTed there. Then a *test harness*
  stack pinned `MAX_DEPTH` so Chezzi refused 30 nested parens where CPython takes 200 — put test
  callers on the production stack (`on_frontend_stack`), bisect bytes-per-node per phase, derive the
  constant. When realistic depth exceeds the smallest stack, decouple onto a dedicated big stack
  rather than squeezing the cap. A per-`Parser` bound is not global (interpolation re-parses with a
  fresh budget) — enforce on the finished tree. Parse-time recursion ≠ post-parse walker recursion;
  left-associative chains parse iteratively and overflow the walkers instead. Do not buy margin by
  raising `VM_STACK_BYTES`: it is reserved per M:N worker.
- **Opt-in caps whose measure is per-heap or wall-clock are non-comparable across execution contexts.**
  Do not design one expecting two contexts to agree.
- **Pure-Chezzi scanners must pre-collect codepoints.** `text[i:i+1]` re-collects the whole `Vec<char>`
  per call (O(n²)); `field = field + c` is O(k²). `std.csv` hung on a 20k-field row. Verify with a
  large-input timing test.

## 5. Native surface and stdlib

- **A new builtin type/ctor/fn goes in its owning `std.*` module, import-gated, never the global
  reserved namespace.** A global name is a one-way ratchet; a five-task cleanup existed only to undo
  earlier ones. Gate is checker-only name resolution; a pure type/ctor also needs the `bind_import`
  skip or `import X from M` faults at runtime — cover with a test that *runs*.
- **Qualified paths are exactly two-level** (`net.Socket`, never `std.net.Socket`), Go-style, by
  decision. A too-deep path gets a targeted diagnostic keyed on the first two segments.
- **Making a native type first-class under `module.Type` is three additive touch points** (checker
  `Type::Qualified` arm, compiler field-callee arm emitting the same opcode, `bind_import` skip).
- **Builtins are declared as bodyless `native` decls in `std/*.chz`, front-end only.** The boundary: a
  native decl cannot express an arg-dependent result (`print`, container ctors) or a bare scalar alias
  (ffi widths); those stay Rust by nature. Two "why `Iterable` can't port" theories were both false —
  verify the actual got-vs-want `Ty` before asserting a wall.
- **The native seam is scalar/str/map-shaped.** Generic `list[T]` cannot cross; generic helpers are
  pure-Chezzi in a `std/*.chz` calling native scalars. Concrete `list[str]` crosses by cloning the
  `map[str,str]` triad through all hosts. A native module name short-circuits a same-named `.chz`.
- **Type args go where the generic is declared** (`Box[int].Has(5)`, `obj.m[U](x)`); the old
  `Enum.Variant[T]` gliding is removed. The parser cannot tell `Box[int].make[U](x)` from
  `value[i].field[k](x)`; only the checker can, by reinterpretation — never a parser steal.
- **Static protocol requirements via dictionary passing are shelved**; a factory closure is the working
  alternative. A no-`self` protocol requirement is declarable but a dead marker.

## 6. Test infrastructure

- **Testing is hybrid: Chezzi first** (`tests/chz/`, run at two worker counts), Rust `#[cfg(test)]`
  only for what `assert` cannot express. Fault paths are Chezzi-able via `recover:`.
- **Order assertions: exact when causally forced, multiset when genuinely concurrent, never
  blanket-sorted.** An in-process capture splices task output in spawn-slot order while the CLI streams
  line-atomically, so check which sink the test uses. External CPU contention masquerades as flakiness
  (a second build turned a 28 s suite into 257 s and 16/21 green) — check `uptime`/`pgrep cargo` first,
  and never relax an assertion until the flake stops.
- **A test needing ≥2 free pool threads or a wall-clock assertion belongs in `tests/` as its own
  binary.** The lib suite shares one process-wide `OnceLock` pool; forcing a worker count inside it
  either no-ops or starves the run (measured: >54 min unfinished at one thread).
- **Every test touching process-global native state takes the module's `#[cfg(test)]` mutex** — one
  unlocked participant defeats the lock for all the others (2–6/40 red).
- **A wall-clock perf repro must be red on the *release* binary the gate builds.** A quadratic path is
  ~10× cheaper in release; 8 000 chars / 0.3 s passed on base. Size inputs at 100k+, one absolute
  bound, never a ratio of two clock samples (`tests/no_wall_clock_ratio_gates.rs` bans that).
- **`live_bytes` under-measures a `Value`-size change** (it sums 88 B per `Obj` slot and ignores the
  operand stack). Gate value-model changes on bench deltas and peak RSS.
- **The CPython differential's integer-bound tracking must follow a value across every seam** (loop
  back-edge, call boundary), or a Chezzi overflow fault reads as a false-positive "bug". Both leaks
  were at seams; one shipped past a 3000-seed clean sweep. Regression pattern: hand-build the
  worst-case IR and assert the oracle reports it.
- **Mechanical oracles found nothing across 23 000 panic-fuzz and 13 000 differential seeds in wave
  9**; every finding came from a hand-built program judged against a run reference. A flake comparison
  must be sampled — a ~5% rate can read as `0/30`.
- **The LSP is validated in headless nvim against a reinstalled `chezzi-lsp`**, not cargo alone; the
  installed binary is a snapshot. Decl-site hover is an additive `Span` + `hover_record_at`, gated on
  `!generic_arg_prepass` or it latches `Unknown`.

## 7. Process and git hygiene

- **Build the repro first, then price a filed gap.** Both W7-4 residuals were mispriced in the
  expensive direction because their premise had drifted under later commits; a "too expensive /
  unreachable" note is the single most load-bearing sentence in a ledger and the least revisited.
  Read a closed row's prescription before re-implementing it: W8-2's and W8-7's filed fixes were both
  measured wrong.
- **Merge ritual:** confirm `HEAD` is `main`; `git merge-base main <branch>` must equal `main` (a
  branch 24 commits stale reported "green" on the stale base); build via `cargo run --bin chezzi` never
  a hard-coded target path; delete the branch and prune the worktree (each `target/` is ~1.6 G and a
  session hit 0 bytes free).
- **Two concurrent builds must not share a release target dir** — one overwrites the other's binary,
  cargo says "up to date", and you verify a binary that silently lacks your change. Confirm with
  `strings … | grep <new message>` when in doubt.
- **Automated review signals are not a merge gate.** Self-review false-dismisses; a panel can diff the
  wrong ref; a verdict can omit its own confirmed charges; remediation can flip-flop; a self-fix needs
  its own review; the post-merge gate never ran its panel. But read the charges — the panel produced
  true positives nothing else caught (`import std.str` destroying the global `str()` ctor). The single
  arbiter is manual repro on the merged binary at both worker counts, against the ancestor.
- **Subagent briefs need a mapped seam and explicit stop conditions.** A vague brief stalls or ships a
  plausible-but-wrong diff; a grounded one (file:line anchors, invariants, "STOP if this needs a checker
  change") shipped 13/13 including subtle soundness. Put "no subagents, no monitors, no sleep, never two
  cargo commands in one worktree" verbatim in every dispatch — one implementer spawned two of its own
  and left 50 monitors firing for hours.
- **Wrap concurrent `cargo` in a memory-capped scope** (`systemd-run --user --scope -p MemoryMax=6G`).
  Exit 137 means "reduce parallelism", not a test failure. Run salvage tests under a shell `timeout` —
  a hang reads as exit 0.
- **Docs are part of the change.** Grep for `deferred` / `experimental` / `not yet` / `follow-up` about
  what you changed and fix it in the same commit. A green build with stale docs is not done.
