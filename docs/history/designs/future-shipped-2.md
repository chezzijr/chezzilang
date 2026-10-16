> Source: `docs/design/roadmap.md` (was `docs/future.md`) lines 977-1860 (§3a1-§4), moved verbatim 2026-10-16. The open items now live in `docs/design/roadmap.md`.

## 3a1. Generics strategy — the live options, and the ruling (2026-08-09; **BUILT as M24, 2026-08-10**)

Items 13 (`T.default()`), 14 (`cast[T]`) and 15 (`Convert[S]` slice 3) above were all blocked by the
same wall, and §14's "erasure contract" is what the wall is made of. This section records the
**strategy decision that governs all three**, so they stop being re-litigated one at a time.

> **Status: the ruling shipped.** Witness passing is built (**M24**, 2026-08-10) and items 13 and 15
> are closed by it; item 14 (`cast[T]`) stays closed for its own reason (no consumer). The "what stays
> impossible" list below is still true and now has one more entry — the **fn-value wall** — which is
> the only one of the six axes the spike named that did not close. Everything below is kept as the
> reasoning of record; the "recommended next step is a SPIKE" note at the end is superseded.

### The question

Chezzi's generics are **erased**: `T` exists in the checker and is gone by the time the compiler
emits. Nothing at runtime knows a `Box[int]` from a `Box[str]`. Three ways forward:

| | monomorphize | witness passing | stay fully erased |
|---|---|---|---|
| `T.default()` / `T.convert()` through a bound | ✅ | ✅ | ❌ |
| `cast[List[int]]` / `match` on a `T` | ✅ | ❌ | ❌ |
| unboxed `List[int]` (`Vec<i64>` not `Vec<Value>`) | partial | ❌ | ❌ |
| one erased body per generic fn | ❌ | ✅ | ✅ |
| separate compilation / `import` | breaks | fine | fine |
| size of the change | whole-language | one milestone | none |

### Ruling: **witness passing.** Monomorphization is NOT the direction.

Reasons, in the order that decided it:

1. **Its biggest win is a feature we declined.** Monomorphization's headline gain is reified type
   args — `cast[List[int]]`, `match` on a `T`. But `cast` is dead: it only pays off through the `Any`
   top type, and **the owner ruled against leaning on `Any` (2026-08-09) — "we vouch for statically
   typed; `Any` is Go's `interface{}`"**. With `Any` out, §14 has no consumer.
2. **It does not deliver the perf win people assume.** `Value` is a uniform 8-byte pointer-tagged word
   (`src/vm/value.rs`), so a monomorphized `List[int]` is still `Vec<Value>`. Unboxing needs *typed
   containers* as a separate change. Monomorphization alone moves nothing on the CPython gap — do not
   justify it on perf without building the container work too.
3. **It contradicts the declared anchor.** `PROGRESS.md` pins the reference model as **Java (erased)**
   precisely because Java is the only mainstream *erased* model, and copying Rust/Swift ergonomics
   without their machinery is how generics go inconsistent. Monomorphizing is not an increment on that
   model; it replaces it.
4. **It breaks the module story.** Chezzi resolves modules at runtime (`resolver`), so there is no
   whole-program point at which every instantiation is known.

**The one condition that would reopen this: the Cranelift JIT.** A JIT wants monomorphized,
type-specialized code, so if that end-game is committed to, monomorphization is on its path. `CLAUDE.md`
scopes Cranelift as a late-stage endeavor, so this stays closed until then — but revisit it *with* the
JIT, not before, and revisit it as one decision rather than as three feature requests.

### What witness passing must satisfy before it ships

The mechanism is small — resolve the conforming type's static method at the call site, thread it in as
a hidden trailing argument, keep the single erased body. **The mechanism is not what failed twice.**
Both rejected runs (branches `auto-task/protocol-static-req`, `…-v2`, item 13 above) died the same way:
the checker's "accept" boundary drifted out of lockstep with the compiler's "can-lower" boundary, so
each run half-covered the surface and a prosecutor found the next axis.

A witness cannot be resolved once at the outermost call, because the caller is often generic itself:

```chezzi
fn make[T: Default]() -> T: return T.default()
fn g[T: Default]() -> T:    return make[T]()     # T is STILL abstract here
```

`g` must *forward* its witness. So the deliverable is **one checker gate that decides "this call can be
witnessed", proven against every axis at once** — not codegen. The six axes that broke the earlier runs,
which any future attempt must cover before writing a line of lowering:

1. cross-module call
2. `spawn:` / `parallel:` body
3. first-class value / `defer` (`g := make; g()`)
4. inferred `T` through a container (`xs: List[T]`)
5. non-leading bound param
6. generic-calls-generic witness forwarding (the shape above)

**M23 is evidence the contract is now expressible.** The `Eq` protocol needed exactly this shape and
landed it: `validate_eq_shape` (checker decides) → `binds_eq_hook` (compiler records) →
`Program::eq_struct` / `eq_enum` (VM dispatches from a validated table, never a name lookup). Checker-only
was tried and rejected there for the identical reason — `fn same[T](a, b): return a == b` erases `T`, so
a use-site gate never sees the concrete type. That is the first in-tree precedent for a
checker→compiler→VM contract over an erased boundary.

**The spike ran, and the contract closed — M24 (2026-08-10).** Five of the six axes are supported;
axis 3 (first-class value / `defer f(...)`) is a permanent wall and is listed below. The shape is
exactly M23's: the checker decides ([`Checker::witness_params_of`] answers "does this fn take hidden
witness params" ONCE and stores it on `FnSig::witness_params`; every consumer reads it from there),
the compiler records (`$w:T` locals, appended to nested bodies' capture entries), and the VM
dispatches from the recorded key (`Op::CallStaticDyn` → `Vm::do_static_call`). The lesson from item
13's shelf note held: **a filed residual's premise decays** — the note was written 2026-06-25, before
any of this machinery existed, and re-deriving it is what turned a "not worth the cost" into a
milestone.

**One trap this milestone paid for three times, worth stating generally: a span used as a cross-half
table key must not double as a diagnostic anchor.** The witness table is keyed by source position so
the checker's answer and the compiler's lookup agree. Three separate bugs on this branch were the
same mistake — `|>` desugars at PARSE time and gives every link of `a |> f() |> g()` the span of the
whole infix expression, so two witness calls aliased onto one key and the second silently took the
first's type: a **wrong value both engines agreed on**, so parity was blind to it and only a running
test with two different concrete types caught it. The third instance survived a fix that *claimed* to
have separated them: the fragment anchor was still written onto a cloned AST node's `span`, so a
fragment root that was itself a string carried the outer literal's span into the NESTED
interpolation's keys and both per-call tables missed. A FOURTH followed the third — a re-lexed
fragment carried an absolute line but no absolute COLUMN, so two sibling nested fragments restarted
at column 1, shared one key, and both took the last one's witness (that fix turned a loud fault into
a silent wrong value). The key is now the **callee token** (distinct per link) and the anchor is
**deleted**: a fragment is re-lexed against the enclosing literal's `PosMap`
(`lexer::tokenize_frag`), so every fragment token span is the char's REAL physical source position,
and with a real position there is nothing left to anchor — the checker points at the expression,
exactly where CPython carets inside an f-string (measured 3.14.6). The general lesson holds: any
future checker→compiler table keyed on position inherits this hazard, and the durable fix is to make
the position REAL rather than to correct it at a consumer. **M24-6 (2026-08-11) is that lesson
carried to its conclusion.** The intermediate fixes above each made the position *more* injective by
a chosen arithmetic — an absolute base line, then a base column that deliberately kept counting past
a newline (`Lexer::base_col`, since deleted, whose price was a column that could run off the end of
its physical line). The map replaces the arithmetic with the thing itself: keys are injective because
two distinct source chars are two distinct positions, a property of the FILE. Same move `W7-49` made
for module identity in the other axis.

### What stays impossible under witness passing, permanently

State these when the questions recur; they are not oversights:

- **A witness-taking generic read as a FUNCTION VALUE.** `g := reset`, a turbofish read as a value, a
  HOF argument and a cross-module read all reject with *"'reset' cannot be used as a function value:
  its bound on T requires a static protocol method, which needs the concrete type — a function value
  erases it."* A `fn` value carries a code pointer and captures; it does not carry which *declaration*
  it came from, so there is no site at which the hidden argument could be supplied. This is axis 3 of
  the six above — the one that did not close, and the reason to say so plainly is that it looks like a
  v1 gap and is not. Workaround: call it directly, or take a factory closure
  (`fn make[T](mk: fn() -> T) -> T`).
- **A type parameter of the enclosing TYPE** (`struct Bx[T: Default]` … `T.default()` in a method).
  The concrete type is erased the moment a `Bx` *value* exists, so only a value could carry the
  witness — and putting it there would make every generic struct pay for it. Declare the parameter on
  the **member** instead; its witness rides on the call.
- **`cast[List[int]]` / any parameterized downcast.** `Obj::List` carries no element type. Java refuses
  the same thing at compile time (`o instanceof List<String>` → *"Object cannot be safely cast to
  List<String>"*); Python refuses it at runtime (`isinstance(x, list[int])` → *"argument 2 cannot be a
  parameterized generic"*). Both measured 2026-08-09.
- **`match` on a bare `T`** — `cannot match on non-enum type T`.
- **A generic slot that coerces like a declared one.** `fn f(x: float)` accepts `f(2)` → `2.0`; a method
  param declared `T` on a `Box[float]` rejects `b.set(2)`. Same type, same value, different answer,
  because the backend sees `T` and not `float`. Documented at `docs/reference/spec.md:513`; the erasure tax
  showing through to users, and the one item on this list that is arguably a bug rather than a wall.
- **Specialized numeric containers.** Needs typed containers, independent of this decision.

**Never on that list — the `spawn f(...)` / `defer f(...)` STATEMENT TARGET was a DEFERRAL, and it is
now FIXED** (2026-08-14, `docs/contributing/gaps.md` **M24-5**): the six emit lines push the hidden witness after
the declared args and widen `Op::SpawnCall`/`SpawnMethod`/`DeferCall`/`DeferMethod`'s `argc`, which
is all it ever needed — the VM was already argc-generic. It was arity plumbing, not erasure. The
receiver-less head found beside it (`defer Holder.build(3)`, which PANICKED the compiler) is fixed in
the same row: a static method is an ordinary call, so every static spelling now lowers through a
wrapper proto that replays it — still no new opcode, still no VM change.

### Not the same problem — do not fold it in

**Compiler type-blindness** (`docs/design/roadmap.md §4`, struct-field slotting) looks like erasure and is not.
The compiler discards *all* static types, not just generic args — it knows a field's *name* but not the
receiver's struct type. The fix is to thread the checker's types into the compiler, and its payoff is
larger than perf: it is the `checker-superset-of-compiler` soundness class, where the checker accepts
what the compiler cannot lower. Keep the two tracked separately.

---

## 3b. Test system — planned improvements (M20 shipped; these are follow-ups)

M20 shipped: `assert`, free `test fn`, struct **suites** with lifecycle hooks
(`before_all`/`after_all`/`before_each`/`after_each`) + a typed shared fixture, and the `chezzi test`
runner (`src/test_runner.rs`). The native suite lives in **`tests/chz/`** (`spec/`/`stdlib/`/`suites/`),
run on the M:N engine (the only engine), with a `cargo test` gate `chz_suite_passes`
(`tests/chz_suite.rs`) asserting the whole suite passes and `tests/chezzi_threads_cli.rs` re-running it
at `CHEZZI_THREADS=2`. These are the ranked follow-ups.

Current semantics: `assert true`=PASS, `assert false`=FAIL (with `file:line:col` + message; the message is
any `str` **expression**, not just a literal — variable/interpolation/concat all work, checker-enforced
`str`). **Any other runtime fault renders ERROR** (see item #1 below). File-level compile/type errors
render ERROR too (whole file, before any test runs), counted separately as `file error(s)`.

1. **FAIL vs ERROR split — DONE.** A `test fn`/method is **void**, so `assert` is the *only* intended
   failure signal → any other runtime fault (OOB, div-zero, overflow, missing key, native fault) is by
   definition **unexpected** and renders **ERROR**, not FAIL — pytest's FAILED-vs-ERROR distinction
   ("wrong assumption" vs "code crashed"). **Landed:** `RuntimeError` (`src/vm/mod.rs`) carries a
   `pub is_assert: bool` discriminator (default `false`; `Display` unchanged, so error strings stay
   byte-identical), set `true` ONLY by the `Op::Assert` arm (`src/vm/exec.rs`). The runner's per-test
   `Outcome` now holds an extensible `Verdict` enum `{ Pass, Fail{line,msg}, Error{line,msg} }`
   (`src/test_runner.rs`): a free-test / test-method body fault routes assert→`Fail`, else→`Error`, and
   every setup/teardown fault (suite construction, `before_all`/`before_each`/`after_each`) is
   `Error`-class regardless of `is_assert`. Summary is now `P passed, F failed, E errored` (+ optional
   `K file error(s)`); `report.passed` requires `F==E==file_errors==0`; exit non-zero if any. The
   `Verdict` enum is the extension point for the ergonomics wave's `TimedOut`/`OverMemory` buckets —
   `OverMemory` is now the first of these to land (item 1b below).

1b. **Per-test memory cap (`--max-heap=<bytes>`) — DONE.** A runaway-allocation guard for `chezzi test`,
   mirroring the (still-planned) per-test timeout but for memory. Opt-in `chezzi test --max-heap=<N>`
   (plain byte count; `0`/omitted = OFF, the default): when a single test's in-VM live heap exceeds `N`
   it is **hard-aborted** (bypassing `recover:` — a `for: recover: <alloc>` loop cannot defeat it) and
   bucketed in a new `Verdict::OverMemory` (rendered `OVER-MEMORY name (file) msg`, counts as failure,
   exit non-zero; summary appends `, M over-memory` only when `M>0` so cap-off output is byte-identical
   to before). **Deterministic-in-VM, NOT OS RSS:** the cap is checked against `Heap::live_bytes()` —
   the same value already computed once per `sweep()` for the peak probe — a per-heap high-water, so it
   is deterministic and the `chz_suite_passes` gate (which runs cap-OFF) is
   untouched. Mechanism mirrors the cancel bypass-recover unwind: `Heap` gained `mem_cap`/`over_cap`;
   `sweep()` sets `over_cap = mem_cap != 0 && lb > mem_cap`; `run_until`'s post-collect boundary
   hard-aborts via `unwind_deferred(base_level, false)`. The check is **re-observed at every GC boundary
   like a cancel checkpoint — no latch** — so a `defer` that itself allocates runaway during the abort's
   own cleanup unwind is bounded too (its nested `run_until` re-trips and aborts it; `should_collect()`
   resets after each collect, so a non-allocating defer runs to completion). The abort stamps an
   **`is_over_memory` marker onto the `RuntimeError`** (mirrors `is_assert`, excluded from `Display` so
   golden output is unaffected) and forces it back on after every unwind, so it travels WITH the error across
   an enclosing **native-reentry** `run_until` (a HOF callback / operator overload / deferred call) AND a
   **`spawn`'d worker's** fault crossing back to the parent; the `run_until` Err funnel bypasses
   `recover:` whenever the marker is set, and `verdict_from_fault` reads `e.is_over_memory` first to
   bucket it. `spawn`/`parallel:` tasks are covered on M:N too — `spawn_worker` threads `mem_cap` onto
   the worker's own heap, so a **runaway** alloc in a task trips and buckets it the same way. VM + runner
   + `main.rs` flag only — no checker/compiler change. **Off-heap wire storage IS counted (gaps.md
   W6-10, fixed 2026-07-27).** A value moved across the airlock into a `Channel`/`Shared`/`RwShared`/
   `Atomic`/`Executor` core lives as a `WireValue` in an `Arc` **outside every `Heap`**, so `live_bytes`
   used to count it nowhere and a 195 MB channel backlog sailed past a 200 KB cap. Each core now caches
   its payload's approximate byte size (the same summary that makes the GC skip pure-data payloads —
   see gaps.md W6-7) and `live_bytes` adds it in, so the natural *concurrent* runaway — an unbounded
   backlog, or data parked in a `Shared` — trips like any other. A core's bytes are charged **once per
   core per heap** (by `Arc` pointer identity): `from_wire` mints a fresh `Obj::Shared`/`Obj::Channel`
   alias slot on every crossing, so charging per *slot* would multiply one payload by the number of live
   handles and fire OVER-MEMORY on a program using a fraction of the cap. Two things follow, and they
   sharpen the per-heap guarantee below rather than restate it: a core reachable from N M:N worker heaps
   is counted in **each** of them (the number is "bytes **reachable from** this heap", not an ownership
   split — the N heaps' totals do not sum to RSS), and a payload reachable only through a **nested** core
   whose last alias slot has been swept needs a cross-core recursion to be seen at all — that was
   `W6-10r`, **FIXED 2026-08-06**: `live_bytes` now walks into nested cores (`Arc`-de-duped against the
   same per-heap set, so a nested core with an alias slot of its own is still charged once), gated on
   `mem_cap != 0` so a cap-off run pays one branch and zero extra walks. The inline-scalar escape below
   is a different hole, and is **CLOSED 2026-08-07 by `W7-28`** (see the end of this item).
   **GC pacing is byte-aware WHEN A CAP IS SET (round 3, 2026-07-27).** Counting the off-heap bytes was
   not enough on its own: `over_cap` is evaluated only inside `sweep()`, and `sweep()` used to run
   purely on `Obj`-count growth, so a program pushing megabytes across the airlock while allocating ~2
   `Obj`s per iteration never swept, never sampled the cap and **passed** (304 MB against an 8 MB cap;
   a 200k-int sibling at 3369 MB). `should_collect()` now also fires on charged off-heap bytes —
   `mem_cap != 0 && since_gc_wire_bytes >= (mem_cap/4).max(64*1024)`, charged at `Vm::to_wire_crossable`
   (the one helper every cross-heap value store routes through) and reset in `sweep()` beside
   `since_gc`. It is a monotonic pacing HINT, never accounting (`live_bytes` stays the sole measure):
   a replacing store charges, a `recv` never decrements — net tracking would let a steady send/recv
   pipeline stall the trigger forever, i.e. fail open again. **Gated on `mem_cap != 0`**, so cap-off
   pacing (every `chezzi run`, every bench, the whole test suite) is bit-for-bit unchanged; a capped
   run pays extra sweeps plus a second `wire_summary` walk per store (+11% measured, `docs/contributing/benchmarks.md`).
   Residual SAMPLING escapes were listed in gaps.md `W6-10s`, which is **CLOSED 2026-08-06** — its
   filed premise (uncharged by-hand airlock paths) did not survive re-derivation: the only one storing
   persistently off-heap through the QUEUE is `Executor.submit`, and only on the cooperative engine
   (removed 2026-08-16), which `--max-heap` refused at the CLI. (That sentence read "the only one storing persistently off-heap"
   until `W7-26` measured its blind spot the same day: on M:N `submit` runs EAGERLY, so the same call
   stores persistently in the core's *other* half — `eager` — which nothing counted and nothing
   sampled. Both are fixed; `W7-26r` tracks what still is not sampled.) The reachable escape from
   this row was a worker heap **born big** — its task's payload arrives in ~7
   `Obj`s, so the object-count trigger never moves and nothing ever samples — now fixed by
   `Heap::request_collect` from `Vm::spawn_worker`. **Both residuals on that row are now CLOSED**: (b)
   by `W7-28` (next paragraph), and (a) — a task whose entire body is ONE native call, which pushes no
   frame and so never reaches `run_until`'s loop — by `W7-29` 2026-08-07. `Vm::start_task` samples the
   cap before dispatch with the pending call's operands rooted on the operand stack; the filed claim
   that that window had "no safe sample point" was wrong. `request_collect` stays for the OTHER task
   door, `ReadyWorker::invoke` (eager `Executor` jobs), which does not route through `start_task`.
   **THE TRIGGER COUNTS BYTES, NOT EVENTS (round 4, `W7-28`, 2026-08-07).** Every earlier trigger
   counted an event — allocations, wire crossings — and each event class has a shape that adds
   unbounded bytes without raising it. Measured against `--max-heap=8000000`, all PASS pre-fix:
   `xs.push(i)` × 80 M grows the `Vec` behind an existing `Obj::List` and moves NOTHING (**617.8 MB,
   77× the cap**); `big.extend(chunk)` × 150 does the same in ~1200 instructions (**~240 MB**);
   `s = s + s` × 22 (41 MB) and `"x".repeat(20000000)` (20 MB in ONE allocation) both stay under the
   256-object floor. `Map`/`Set` fail open like `List`. So `should_collect()`'s byte term is now fed by
   **all three** funnels through which a heap can gain bytes — `Heap::alloc` (new objects),
   `Heap::get_mut` (the sole `&mut Obj` door, charging a deferred before/after delta, so growth in
   place cannot escape whatever new container method is added later) and `Vm::to_wire_crossable`
   (off-heap, as before) — all reset in `sweep()`, all gated on `mem_cap != 0`. An INSTRUCTION TICK was
   tried first and rejected: `extend` adds N values in one instruction, so no instruction interval can
   bound it. A proxy that is only *nearly* proportional to bytes is not a byte counter.
   **v1 limits (deterministic, documented):** the
   trip fires only at a **GC boundary**; the check is a high-water on `live_bytes` which
   **undermeasures** true RSS and can overshoot ~2× `N` before firing (`next_gc = 2*live`). **The cap is
   PER-HEAP, so its guarantee is: any single execution context (the test's own heap; a `spawn`'d worker's
   heap on M:N) whose live heap — including the off-heap wire payloads it can REACH, which a worker that
   allocated nothing itself may still hold a handle to — exceeds `N` is aborted — a real runaway trips on whichever heap runs it,
   the SAME verdict for a real runaway.** **Historical restriction, now moot:** while the cooperative
   engine existed, `--max-heap` errored if combined with `--serial`, and that is what made the cap
   sound-by-construction. That engine shared ONE heap across the parent + every `spawn`/`parallel:`
   fiber (so its `live_bytes` was `parent-baseline + Σ live tasks`), while M:N isolates each worker on
   its own fresh heap. A *concurrent* test near the boundary would therefore have bucketed `OverMemory`
   there yet passed on M:N. The only cross-engine-identical aggregate would have been a global RSS-style
   measure, which is non-deterministic (rejected — it would break the very gate the cap protects).
   `--serial` was **removed 2026-08-16** (§2b), so the cap is now simply per-worker/per-context and
   fully deterministic, with nothing to disagree with it. `chezzi run` never sets the cap
   (test-runner-scoped). k/m/g
   suffixes and `chezzi run --max-heap` are deliberately out of scope (later waves). (`--timeout` — the
   wall-clock sibling — has since landed; see #4 below.)

2. **Table-driven subtests (`t.Run`-style).** Today a `for case in cases:` loop inside a `test fn`
   aborts at the first bad case with no per-case verdict. A subtest construct that reports each case
   PASS/FAIL (Go `t.Run`, Rust `rstest`, pytest `parametrize`) is the single highest-value ergonomic
   add and fits the Go lineage. Design open: a `subtest "name":` block, or a `cases`-driven helper.

3. **skip / xfail.** Conditionally skip a test (Go `t.Skip`, pytest `skip`/`xfail`, Jest `.only/.skip`)
   — WIP + platform-gated tests. Cheap; needs a skip signal the runner counts separately (`S skipped`).

**Runner ergonomics + CLI options & output format** (independent of the semantics above; `cmd_test` is
`src/main.rs`, accepts `[path]` + `--max-heap` + `--timeout`; the worker count comes from
`CHEZZI_THREADS`):

4. **Per-test timeout (`--timeout=<ms>`) — DONE.** The wall-clock sibling of `--max-heap` (#1b), same
   arc. Opt-in `chezzi test --timeout=<MS>` (ms; `0`/omitted = OFF, the default): a test running longer
   than `MS` is **hard-aborted** (bypassing `recover:`) and bucketed `Verdict::TimedOut` (rendered
   `TIMED-OUT name (file) msg`, counts as failure, exit non-zero; summary appends `, T timed out` only
   when `T>0` so cap-off output is byte-identical). The abort stamps an **`is_timed_out` marker onto the
   `RuntimeError`** — the exact machinery `is_over_memory` uses (excluded from `Display` so golden
   output is unaffected; forced back on after every unwind so it crosses native-reentry `run_until` + the
   worker→parent boundary; the `run_until` Err funnel bypasses `recover:` whenever it is set;
   `verdict_from_fault` reads it first). **Observation site (the key difference from `--max-heap`):** the
   deadline is checked at the **loop back-edge** in `jump_checked` — the hottest engine-independent
   checkpoint, hit every iteration of any `while`/`for` — so it covers BOTH the top-level test body
   (`invoke_test → run_proto → run_until`, which runs OUTSIDE the fiber scheduler) AND a `spawn`ed task's
   loop (which routes through the same back-edge). A per-VM `deadline: Option<Instant>` is armed
   (`now + timeout_ms`) at each invoke entry and threaded onto M:N workers as the SAME absolute instant.
   **Zero overhead when off:** the check is `if let Some(dl) = self.deadline` FIRST, short-circuiting
   before any clock read, so a cap-off `chezzi run`/`chezzi test` does ZERO added `Instant::now()` calls
   on the hot path; when on, the read is throttled to one per 1024 back-edges (a wrapping counter).
   **A wall-clock trip is non-deterministic**, which is why the flag is opt-in and OFF by default; the
   `tests/chz` gates all run timeout-OFF and are untouched by it. (While the cooperative engine existed,
   `--timeout` additionally errored if combined with `--serial`; that engine was removed 2026-08-16.)
   VM + runner + `main.rs` flag only — no checker/compiler change. **v1 limits (documented, watchdog-
   thread follow-up):** the trip is observed only at loop back-edges (+ the M:N reds checkpoint), so a
   test blocked in a **native call** (a blocking syscall, `Channel.recv` with no traffic) or spinning in
   **loop-free infinite recursion** (which hits the stack/recursion guard instead) is NOT caught by this
   v1 — a true watchdog thread that can interrupt a blocked native is the natural next seam. Ms
   granularity; the abort lands at the next back-edge after the deadline (sub-ms overshoot for a tight
   loop). k/m/g suffixes and `chezzi run --timeout` are out of scope.
5. **Name filter. SHIPPED (2026-07-24).** `chezzi test -k <substr>` / `--filter <pat>` runs only tests
   whose displayed name (free `fn_name`, suite `Suite::method`) contains the substring, like
   `cargo test <filter>` / pytest `-k` / `go test -run`. Filtered after discovery, at the invoke site
   (filtered tests genuinely don't run). The summary notes `(K filtered out)`; a zero-match run is a
   clear `— no tests matched '<pat>'` failure (not a silent "0 tests"). Substring, not regex (v1).
6. **stdout capture option. SHIPPED (2026-07-24).** `--show-output` surfaces a FAILING test's captured
   stdout, indented under its `FAIL`/`ERROR`/etc. line (pytest show-on-failure). Default still discards
   (assert-on-value is the intended path); a passing test's stdout is never shown. Kept in the `Outcome`
   only when the flag is on (bounded to the run). **Byte-exact, SHIPPED (2026-08-16).** The captured
   stdout used to round-trip through `String::from_utf8_lossy`, so non-UTF-8 bytes (e.g.
   `io.stdout().write_bytes(b"\xff\xfe")`) rendered as U+FFFD; `Vm::take_out_bytes()` + `TestReport.bytes`
   now carry the raw bytes straight to fd 1, matching `chezzi run` and measured `go test` (go1.26.6) —
   `pytest`/CPython 3.14 deliberately replaces invalid bytes instead (`_pytest/capture.py:492-496`), but
   Chezzi already commits to byte-exact on `run`, so `test` followed to stay self-consistent. `W6-9r`
   item 4, `docs/contributing/gaps.md`.
7. **Better options + output format. SHIPPED (2026-07-24).**
   - Verbosity: `-q`/`--quiet` (dots `.`/`F`/`E`/`M`/`T` + summary only) vs default per-line vs
     `-v`/`--verbose` (per-line + per-test `(Nms)` timing + a total). `-q`/`-v` are mutually exclusive.
   - `--errors=json` machine output, mirroring `chezzi check --errors=json` (same flag; suppresses the
     human lines). Shape necessarily diverges (it carries totals): `{"tests":[{name,file,line?,status,
     message?,duration_ms}…],"file_errors":[{file?,line?,col?,severity,message,help?}…],
     "totals":{total,passed,failed,errored,over_memory,timed_out,filtered_out,file_errors}}`.
     (`message` since 2026-08-18 — the failure text, present on every non-`pass` verdict and absent on
     a pass, so a runner can say *why* a test failed and not merely that it did.) (`file_errors` array
     since 2026-09-06, TICKET-067/W10-9 — before, a whole-file diagnostic that stopped a `*_test.chz`
     file only bumped `totals.file_errors`; the message itself never reached the JSON document. The
     array's keys mirror `check --errors=json`'s minus `end_line`/`end_col`; `file`/`line`/`col` are
     omitted, never guessed, when the failure has no module coordinate.)
   - Color (`--color=auto|always|never`, default auto = isatty on stdout) on the verdict tag. Resolved
     to a bool in `cmd_test`; the runner never probes the tty, so the captured (non-tty) test harness +
     the byte-identity gate never see ANSI.
   - Per-test + total **timing** — `-v`/json ONLY, NEVER in default/quiet output (non-deterministic → it
     would break the byte-exact `chz_suite_passes` gate).
   - `--fail-fast` (stop at the first non-pass verdict) for tight iteration loops.
   - Ordering is deterministic (sorted files → free tests → suites, all declaration order); documented in
     `chezzi test`'s USAGE + the `run_tests_opts` doc-comment. **Default (no-flag) output is unchanged.**

**Migration note (corrects an earlier claim):** fault-path tests **are** portable in-language via
`recover:` — `r := recover: <faulting expr>` yields `!e` and `e.message()` gives the fault text, so
`assert e.message().contains(...)` tests a fault without Rust (proven end-to-end). The runner keeps
its *own* fault tests in Rust only because IT needs the fault `span` for `file:line`. So the "stays in
Rust" set for the `tests/chz/` migration is just: gc-stress rooting (`run_capture_stress`), checker
`rejects/ok` (compile-time), parser/lexer/bytecode internals, and concurrency timing — **not**
fault-path. Fault-path is a future migration cluster.

---

## 3c. Native-registry hygiene: a native's PROPERTIES belong on its registry entry — **DONE 2026-08-05** (option B, plus the interception fold)

**Landed.** `pub enum Kind { Inline, Blocking, TimedWait, InterceptIo, InterceptNet, HostWait }` is a field of
every `MEMBERS` tuple (`&[(&str, NativeFn, Kind)]`, 192 entries across 14 tables), copied onto
`Obj::Native` when the module binds (`vm/exec.rs`) and carried through `WireValue`/`SnapValue` and
`Callee::Native` into `Vm::invoke_native(func, name, kind, args, span)`. `is_blocking` is **deleted**,
along with its `strip_prefix('_')` patch and the `native_member_names_are_unique_across_modules` guard
whose whole premise was bare-name classification. **No native's behaviour is decided by a string
comparison anywhere in the VM**, and a `MEMBERS` entry without a kind is a compile error
(demonstrated: dropping `Kind::Inline` from `("now", now)` → `expected a tuple with 3 elements`).

Three corrections to the estimate below, all found by reading the code before writing it:

- **`Vm::invoke_native` has exactly ONE call site** and `Obj::Native` is built in 3 non-test places, so
  the kind rides the value to the dispatch site — **no name→kind map, no lookup, no per-call cost.**
  The "~10 tuple-destructuring consumers" were 2 real ones (`vm/exec.rs`, `compiler/mod.rs`) + tests.
- **Name-keying would have been unsound anyway.** `std.io::_append` (an intercepted opener) and
  `std.fs::_append` (a syscall) collide on the bare name; they were kept apart only by the ORDER of the
  checks plus an exemption list in a test. Distinct entries, distinct kinds, no ordering hazard.
- **The interception fold was worth doing in the same pass** (it was listed out of scope). `connect`/
  `listen` were matched by BARE NAME in `invoke_native` — a future `std.foo.connect` would have been
  hijacked by the net handler — and the `std.io` openers by `fn_addr_eq` identity. Both are now kind
  arms. This is still two properties on one enum, not a plugin registry: the "don't grow it" warning
  below stands.

**Found by the conversion, fixed the next day:** `fs._stat`/`fs._walk` were never in the `is_blocking`
list, so they ran inline and pinned an M:N core worker (`walk` recurses a whole tree) — exactly the
silent failure this section predicted, already in the tree. The refactor preserved them as
`Kind::Inline` (behaviour-identical) and filed `gaps.md` **W7-19**; both are `Kind::Blocking` as of
2026-08-05, after the off-heap-safety proof. **This is the section's own payoff, measured**: the
property was made impossible to omit, and the first thing writing it down produced was a live
starvation bug nothing else had detected.

**W7-5e did NOT fold in** — `Vm::stdout_writes` is a per-CALL runtime observation ("did this call emit
to stdout?"), not a static per-native property. It was **fixed separately 2026-08-05** by the same
move this section makes one level down: the property rides the only door that can produce it
(`stream::write_out` takes the writing `&mut Vm` and bumps the counter itself), so forgetting it is a
compile error rather than a silent hole.

<details>
<summary>Original filing (2026-08-05) — kept for the reasoning</summary>

**The registry itself is fine and is not what this is about.** A native ships as a per-module
`pub const MEMBERS: &[(&str, NativeFn)]` (`src/native/<mod>.rs`) plus a bodyless `native fn` decl in
`std/<mod>.chz` for the signature. Two edits, both obvious, both local. Keep that.

**The problem is that a native's BEHAVIOURAL PROPERTIES live in string matches far from the entry.**

| property | where it lives today | what it decides |
|---|---|---|
| "is this blocking?" | `src/native/mod.rs:520` — one ~40-name `matches!` | offload to the dirty pool vs run inline on the worker |
| "is this a timed wait?" | `src/vm/call.rs:291`, `:339`, `:359` — three `"sleep_ms"` string arms | ride the timer thread; be a continuous cancel + `--timeout` checkpoint (W7-16) |

`sleep_ms` is named in 4 files. **A new blocking native that forgets `is_blocking` fails SILENTLY** —
nothing errors, no test goes red, it just pins an M:N worker for the syscall's duration (the D5
starvation the set exists to prevent). That near-miss is already documented inside the function it
would break: `native/mod.rs:521` strips a `_` prefix specifically so the W7-8 rename did not silently
un-classify every `std.fs` syscall.

**The fix — the property moves onto the entry, so the table is the single source of truth:**

```rust
pub struct Native { pub name: &'static str, pub f: NativeFn, pub kind: Kind }
pub enum Kind { Inline, Blocking, TimedWait }   // TimedWait replaces all three `"sleep_ms"` arms

pub const MEMBERS: &[Native] = &[
    Native { name: "sleep_ms", f: sleep_ms, kind: Kind::TimedWait },
    ...
];
```

`is_blocking(name)` becomes a lookup; `match name { "sleep_ms" => … }` becomes `match kind { TimedWait
=> … }`. The payoff is not tidiness — it is that **omitting the property becomes a compile error** (a
missing struct field) instead of a silent behaviour loss.

**Two sizes, and they are genuinely different jobs:**

| | diff | gets you | when |
|---|---|---|---|
| **A. Collapse the scatter** — add `native::kind(name) -> Kind` beside `is_blocking`; `call.rs` matches on `Kind::TimedWait` | ~4 files, ~40 lines | 4 files → 1. Still a name match, so still silently forgettable | safe any time |
| **B. Property on the entry** (above) | **192 entries across 12 tables** (`ffi` 59, `math` 35, `io` 20, `fs` 17, `encoding` 13, `os` 12, `crypto` 10, `request` 8, `process`/`regex` 5, `rand`/`time` 4) + ~10 tuple-destructuring consumers (`native/mod.rs:824,851,873,890,976,1012,1055`; `ffi.rs` tests) | the compile-error guarantee | **before the JIT freeze** — a table-shape change wants that boundary, and it is a pure refactor best done outside a bug-hunt |

**Deliberately NOT in scope.** `timer(ms)` was then an **opcode**, not a native, so it never joined
this table (since 2026-10-16 it is an ordinary `std.time` member, `Kind::InterceptTimer`). Do not grow this into a trait-object or
plugin registry: it is two properties and one special-cased name, and a `kind` field is the whole fix.

**Adjacent, same family, already filed:** `gaps.md` **W7-5e** — `Vm::stdout_writes` is a *third*
per-native property ("did this call write to stdout?") resting on an unenforced invariant. If B is
done, check whether it folds in as a fourth `Kind` / flag rather than staying a hand-maintained
assumption. *(Checked when B landed: it does not — see the header above.)*

</details>

---

## 4. Optimizations (ranked effort → payoff)

> **Live numbers:** `docs/contributing/benchmarks.md` tracks Chezzi vs CPython (reproducible via
> `benches/run.chz`). After the M19 phases (call-flatten + SSO incl.): **~1.3×–3.5× slower than
> CPython**, and a **standing startup win** (~11× faster cold). The gap scales with call density —
> `loop` (no calls) is 1.32×, `fib` (all calls) is 3.54×. The M19 levers below are marked landed;
> the **ranked not-started backlog is "Post-M19 next levers"** further down.

The original M5 baseline was ~4–6.5× over the then-existing (now-removed) tree-walker, near the safe-match-dispatch floor; the current live comparison is vs CPython (see `docs/contributing/benchmarks.md`). The two real costs are
**dispatch count** and **name lookup** — with **per-call allocation** a close third on call-heavy code.

**Cheap — do first:**
- ✅ **Peephole + constant folding (compiler)** — *landed M19 Phase 1* (`src/compiler/peephole.rs`):
  a jump-relocating pass that folds `ConstInt`/`ConstFloat` arith + `Neg`/`Not`, replicating the
  VM's checked semantics (overflow / div-by-zero stay unfolded so the runtime raises the same error).
- ✅ **Superinstructions** — *landed M19 Phase 1*: `BinLocalLocal` / `BinLocalConst` / `IncLocal`
  fuse the hot `GetLocal+GetLocal+BinOp`, `GetLocal+Const+BinOp`, and `i += k` windows (Int fast
  path inlined; non-Int falls back to the exact unfused op). Cut `loop` −36%, `primes` −25%.
  Remaining candidates: `GetLocal+GetField`, fuse compare+`AsBool`, the load-store accumulator.
- ✅ **Global-slotting (inline-cache equivalent for name lookup)** — *landed M19 Phase 2b*: the
  compiler assigns each module global a stable `u32` slot (`ModuleProto.global_slots`) and emits
  `GetGlobalSlot`/`SetGlobalSlot`/`DefineGlobalSlot`; `Obj::Module.globals` (a `HashMap<String,Value>`
  probed by name per read) became `{ slots: Vec<Value>, index }`, so a global read is a `Vec` index,
  no hash. The slot map lives in the shared `Arc<Program>`, so parent and faulted-worker agree on
  slot↔name by construction — removing the slot-order fragility rather than just guarding it.
  **Reality vs prediction:** it moved `fib` −9% (the call-heavy bench resolves its callee per call),
  but *not* `primes`/`loop` — their hot loops read locals, not globals, so the "moves `primes`" guess
  was wrong about where global-read density actually is. Still cheaper on every global read.
- ✅ **Struct-field caching (the other half of name-lookup ICs)** — *landed M19 Phase 4*: static
  slotting (P2b's model) is impossible for fields because the compiler is **type-erased** (it knows the
  field name but not the receiver's struct type at emit time), so `GetField`/`SetField` carry a
  per-call-site IC id into a per-`Vm` `field_ic: Vec<IcCell>` that caches the field index; a hit
  re-verifies `fields[idx].0 == name` and skips the name-probe. The cell holds an index, not a `GcRef`,
  so it touches no GC / snapshot / `swap_ctx` machinery; each access self-verifies, so it stays sound
  under any future polymorphism. **Reality vs prediction:** −13% on a field-access-bound bench
  (`struct`, 3.32×→2.89× CPython), but **~neutral to −3% on a method-bound shallow-field bench** — the
  cold `field_ic` indirection only pays off when field resolution is the actual bottleneck (wider /
  deeper structs), not when method dispatch dominates. **Follow-up — landed M19 Phase 5b (`bbdcb38`),
  measured neutral:** a struct **type-id guard** (stamp a numeric type id on `Obj::Struct`; guard on
  `obj.tid == cell.tid` — a pure-int compare with no name re-verify) replaced P4's name re-verify on a
  hit. It did *not* close the shallow-struct caveat (the cold-IC indirection, not the re-verify, is the
  cost), but was kept: cheaper hot path, VM-only ⇒ behavior-preserving.
- ✅ **Kill per-call clones in `invoke_value`** — *landed M19 Phase 1*: matches on `&Obj` (no whole-
  `Obj` / closure-`HashMap` clone) and drops the arity-check `name.clone()`. Cut `fib` −17%, `list`
  −22%. The one miss: `core_method`'s `Obj::Str` arm still cloned its whole receiver before EVERY
  method dispatch — closed by TICKET-072 (2026-09-08), which moved the five borrow-only methods
  (`len`/`starts_with`/`ends_with`/`contains`/`index_of`) above the clone.
- ✅ **Pass call args as a stack slice (no per-call `Vec`)** — *landed M19 Phase 2*: `do_call`'s
  `Func`/`Closure` fast path runs in place over the args already on the operand stack (`copy_within`
  drops the callee from beneath them), skipping the `split_off` `Vec` alloc + the re-push in
  `push_frame`. Native / non-callable callees keep the `Vec` path (`invoke_native` needs it). Cut
  `fib` −13%.

**Medium:**
- ✅ **Struct type-id guard for the field IC** — *landed M19 Phase 5b, measured **NEUTRAL***. Stamped a
  dense `tid` (layout id) on `Obj::Struct`; the field IC now guards on `obj.tid == cell.tid` (pure-int
  compare) instead of the `fields[idx].0 == name` string re-verify. **But it didn't move the benches**
  (struct 1.02×, method-bound 1.01× — noise): P4 had *already* collapsed the name-probe to a single
  verify-compare, and for short field names that string compare is already cheap, so swapping it for an
  int compare saves nothing measurable. Kept (correct, principled, no regression, future-proofs real
  polymorphic field sites), but **the field-IC lever is spent** — there is no cheaper guard to reach
  for. The "shallow-struct caveat" was a *prediction*, not a measured cost.
- ✅ **Small-string optimization (the real open `str` lever)** — *landed M19 (2026-06-12)*.
  `Obj::Str` now holds a `ChzStr` (`src/vm/chzstr.rs`): strings ≤ `INLINE_CAP` (22 UTF-8 bytes) are
  stored **inline** in the variant (no per-value `Box<str>` heap alloc), longer ones spill to a
  `Box<str>`. `Deref<str>` + `From` impls kept the ~100 `Obj::Str` sites compiling unchanged;
  `size_of::<Obj>()` stayed 88 B (pinned by a guard test). **`str` 217→174 ms, 2.62×→2.10× CPython
  (−20%)**; `list`/`loop`/`fib` neutral. **Note:** "concat / `split` / `+` builder/rope" is *not* a
  benched lever — the `str` bench is `BuildStr` + `,".join`, and `join` already buffers into one
  `String` (`mod.rs:4377`); `+`/`split` aren't exercised. A builder/rope only helps un-benched
  `s = s + x` loops. The pure-int `list` bench is at the **snapshot-parity floor** (both engines
  clone the iterand at `for`; ints are already unboxed) — no safe alloc lever there.
- ✅ **Faster `usize`/`u64` hasher** — *landed M19 Phase 5a*: `MapData`/`SetData`'s `index` (keyed by
  the cached content hash) and `str_intern` (pointer-keyed) swapped SipHash for an in-tree FxHash
  (`src/vm/fxhash.rs`, no dep). **`map` −7%** (3.04×→2.82× CPython; maps were unbenched, so a `map`
  bench was added). **Gotcha:** a naive multiply-only FxHash was **100× slower** — int keys store
  `f64::to_bits` (zero low bits), and FxHash mixes entropy only upward, collapsing hashbrown's low-bit
  bucket index; a splitmix64 finalizer in `finish()` fixed it. (The field/global IC paths don't use a
  HashMap — they're `Vec`-indexed already — so the lever reduced to the map/set + intern paths.)
- ✅ **`ConstStr` interning** — *landed M19 Phase 3*: a per-heap cache keyed by the literal's data
  pointer reuses the already-allocated handle, so a repeated `ConstStr` push is a pointer lookup,
  not a fresh box. (Cross-site compile-time dedup `Op::ConstStr(u32)` is marginal over this.)
- ✅ **Reduce string-op allocations (`stringify`-into-buffer)** — *landed M19 Phase 2*: `stringify`
  appends into a caller-owned buffer, so `BuildStr` reuses one `String` across all interpolation
  parts (cut `str` −5%).
- ✅ **Arithmetic specialization** — *largely shipped via P1 superinstructions*: `BinLocalLocal` /
  `BinLocalConst` / `IncLocal` inline the monomorphic int path for the hot `local op local` /
  `local op const` / `i += k` windows, so the int loops no longer re-dispatch per iteration. A
  general per-op type-guard cache is the only remaining slice, and it overlaps the superinstructions.
- ✗ **Frame pooling** — *low-ROI here*: `CallFrame`'s `deferred` / `defer_markers` are alloc-free
  `Vec::new()` and frames live in a capacity-reusing `Vec`, so there's no per-call frame alloc to pool
  (P2 already killed the per-call args `Vec`).

**Big (separate milestones):**
- ✅ **Flatten the call loop — LANDED (`634c6f5`); the `Arc::clone` warm-up — LANDED (2026-06-12).**
  `Op::Call` now pushes a frame and `continue`s the running `run_until` loop (no per-call Rust
  recursion / per-call `Arc::clone`); `run_proto_in_place` is kept only for native-initiated calls
  (HOFs). The stand-alone warm-up below — **hoist the per-entry `Arc::clone(&self.program)`** to a
  raw `*const Program` borrow (`mod.rs:2095`; sound because `self.program` is immutable + never
  reassigned) — also landed. Post-flatten the remaining entry is per top-level / native-reentry /
  fiber-resume, **not** per call, so it's neutral on the no-HOF standard suite but **1.05× on
  callback-heavy code** (`benches/chz/hof.chz`); see `benchmarks.md`. The original diagnosis follows.
- **Flatten the call loop (diagnosed 2026-06-12 — the top remaining lever for call-bound code).**
  Every Chezzi function call currently **recurses into a fresh Rust `run_until` loop**:
  `Op::Call` → `do_call` → `run_proto_in_place` (`mod.rs:1992`) → `run_until(base_level)`. Two costs
  ride every call as a result: (1) a **native Rust stack frame** per Chezzi call (push + the
  `frames.len() > base_level` bookkeeping + the `paused()`/result re-plumbing on unwind), and (2)
  **`Arc::clone(&self.program)` on every `run_until` entry** (`mod.rs:2115`) — a per-call atomic
  refcount bump+drop that exists purely as borrow-checker tax. fib(30) is ~2.7M calls ⇒ ~2.7M native
  recursions + ~2.7M atomic clones. **This is why `fib` is 3.85× CPython but `loop` is only 1.31×: the
  gap is the *call*, not the dispatch floor** (straight-line code is already near Python-par). `primes`
  (2.50×) is also call-bound and would move. The fix is what CPython 3.11 did for its jump ("zero-cost
  frames"): make the bytecode `Op::Call` **push a frame and `continue` the existing `run_until` loop**,
  and `Op::Return` **pop + push the result and continue** — no Rust recursion, one `Arc::clone` per
  whole `run_until` instead of per call. **Hard part / behavior-preservation risk:** today pause/park (B1/D3),
  `recover:` unwind, and `defer` all lean on Rust-stack unwinding through the nested `do_call`/`?`
  chain. A flat loop must instead park by leaving `self.frames` intact and breaking the loop (the M:N
  engine already saves/restores frame state via `FiberCtx`, so the machinery exists). **Keep the
  re-entrant `run_proto_in_place` for native-initiated calls** (HOFs — `map`/`filter`/`sort` call
  `invoke_value` per element and need the callback's result *synchronously* mid-native-method); only
  the bytecode `Op::Call` path flattens. The two coexist: HOFs nest a sub-loop when they must, the
  common recursive/bytecode call no longer does. Cheap warm-up that stands alone even without
  flattening: **hoist the per-call `Arc::clone`** (raw-pointer/restructure the program borrow) — a free
  few-percent on every call-bound bench. Blast radius is **VM-only**;
  behavior-preservation is testable against the existing fib / recover-in-recursion / defer-in-recursion / deep-
  recursion-overflow goldens. Bigger than a Medium item, smaller than the register-VM rewrite below.
- **NaN-boxing the `Value` — BLOCKED by full 64-bit ints (2026-06-12 reality-check).** The goal
  (16 B → 8 B, operand-stack cache density, moves `loop`/`list`/`fib`) is real, but `Value::Int` is a
  **full `i64`** (`src/vm/value.rs:18`). NaN-boxing packs every value into 8 bytes, and a full i64 +
  a type tag do **not** fit in 8 bytes alongside `f64` — the payload of a NaN-box is ~48–51 bits. To
  do it you must **box big ints** (small-int tagging): a branch + a heap alloc on every int outside
  the taggable range, plus a semantics-sensitive overflow path — i.e. *not* behavior-preserving for
  free, and an uncertain net win on the very int-heavy benches it targets. **Lua 5.4 made exactly
  this call** — it stayed at a 16-byte tagged union *because* it added 64-bit ints. Blast radius is
  **VM-only**, but it's still a milestone-sized design spike (box-big-ints scheme + measure), not a clean
  behavior-preserving session. Park until the int model is up for revisiting.
  - **UPDATE 2026-07-18 — 8B-`Value` is now its own milestone (int-favoring pointer-tag, not NaN-box).**
    Design/plan in `~/.claude/plans/2026-07-18-8b-value-pointer-tag-*.md`: tag `Int` inline (`(n<<1)|1`,
    ±2^62), box the rare wide int (`Obj::BigInt`) and every `f64` (`Obj::FloatBox`). **Phase 0 scaffolding
    landed** (trivially behavior-preserving, additive): `Heap::live_bytes()` + peak high-water probe behind
    `CHEZZI_HEAP_STATS=1` (baseline: `benches/run.chz` peak_live_bytes=24277 at size_of_value=16), and the
    two GC-leaf `Obj` variants (`BigInt`/`FloatBox`, unused by real programs yet — reachable only from a
    unit test; `size_of::<Obj>()` stays 88). Phase 1 (the `struct Value(u64)` swap) is gated on the
    measured memory drop vs this baseline.
  - **DONE 2026-07-18 — the 8B-`Value` swap LANDED (int-favoring pointer-tag).** `Value` is now
    `struct Value(u64)`. The measure gate passed comfortably: on `benches/run.chz` the dispatch-floor
    benches got **faster**, not slower — `loop` 1.13×→**1.03×** CPython (near parity), `fib` 3.29×→**2.95×**
    (first sub-3×), `map`→1.77×, `poly_method`→3.94×; only `primes` +2.7% (in-noise). The cache-density win
    beat the tag decode tax. Behavior-preserving (int `==`/order exact-i64; `1==1.0` preserved; overflow
    still faults at the i64 ceiling). This also surfaced + fixed a pre-existing soundness bug: int `==`
    was lossy `as_f64` above 2^53 (fixed `ccbd3c4`, now exact i64). Full numbers in `docs/contributing/benchmarks.md`.
    Float-constant interning (plan Task 5) deferred — no measured float regression on the int-heavy set.
    **So this lever is no longer "blocked" — NaN-box was the wrong scheme; pointer-tag was the right one.**
- **Register VM** instead of stack — fewer ops, less stack traffic. Effectively a VM rewrite; only
  if dispatch count is still the wall after superinstructions.
- **Generational / incremental GC** — current is stop-the-world full-heap (`next_gc = 2×live`).
  Generational cuts pause + rescan cost on allocation-heavy scripts.
- **Cranelift AOT/JIT** — already the stretch goal. Near-native, but a whole backend. Only after the
  language stops moving.

- **Debug-build M:N stall once the pool covers every free core (lead from `docs/contributing/gaps.md` W12-23, not started).**
  The 10k-fiber D3 program on the DEBUG CLI finishes in 0.3 s at `CHEZZI_THREADS=8` and 1.0 s at 24,
  but runs past 120 s at 27/28 (= nproc) while other cargo builds share the box; the release binary
  takes 0.17 s at 28 beside 20 CPU hogs. Presumably a wait that spins instead of parking. Under a
  4-core `CPUQuota` with 28 hogs the debug CLI did NOT stall at the default pool (11.6–13.6 s), so the
  trigger is whole-box contention, not the quota. TICKET-114 moved the lib test to a fixed pool of 8
  in its own process; the engine is unchanged.

### Memory layout & access patterns (cache levers — diagnosed 2026-06-16)

> **Caveat first (measure, don't guess):** the bench bottleneck is **dispatch + calls + a few alloc
> paths, NOT the value/heap layout** — `loop` is at the match-dispatch floor, and the `struct` bench is
> *method-dispatch*-bound (the field IC already serves hot reads; the type-id guard was measured
> **neutral**). So these are **not** reliable standalone bench-movers. But #1 and #3 below double as
> **JIT-prep**: a method-JIT compiles a field/capture read to a **constant offset**, so it needs a
> canonical *positional* layout — landing them is groundwork for the JIT, not (necessarily) a speedup
> on its own.

**Compact aggregate representation — drop per-instance redundancy (the real layout lever):**

1. ✅ **Shared per-type struct layout (hidden-class / `__slots__`) — DONE (2026-06-16); type-name drop DONE (2026-07-25).**
   `Obj::Struct { tid, fields: Fields }` (`heap.rs`; `Fields` was a flat `Vec<Value>`
   until lever 3b inlined it) stores fields **positionally** (declaration-order offsets) — no per-instance field-name
   strings. Names live only in `StructDef { fields: Vec<String>, tid }` (`op.rs:378`), resolved on the
   cold path (Display/stringify/probe-miss/wire/snap) by `name`→`StructDef`. Killed the N per-field
   `Box<str>` allocs/instance. The single top-level `name: Box<str>` was **also dropped** (2026-07-25,
   memory lever #7): it duplicated `tid`, so it now resolves from `tid` via the dense reverse index
   `Program::struct_names` (`Vm::struct_name_of_tid`, O(1) — the struct analogue of enum `variants_by_id`),
   and struct `==` is now a pure-`tid` int compare (subsumes the old `na != nb` name compare, as enum
   equality compares `variant_id`). The synthetic native structs `Match`/`Response`/`ProcResult`/`FileInfo`
   are registered in `Program.structs` (`compiler/mod.rs` `Compiler::new`) so the runtime can recover
   their field names + type key. Perf: bench-neutral (struct bench reuses instances, dispatch/alloc-bound)
   but a 4-field struct-construction micro went **827 ms → 510 ms (−38%)**, and the name drop is ~28% of
   `many_struct` RSS (measured post-merge). Hands the JIT a constant field offset. Interp removed.
2. **✅ DONE — Enum variant id instead of names.** Was `Obj::Enum { ty: Box<str>, variant: Box<str>,
   payload }` — two `Box<str>` per instance, both global (`Program::variants`). Now `Obj::Enum {
   variant_id: u32, payload }` (the enum analogue of `tid`); the type + variant names resolve from the
   new `Program::variants_by_id` table on the cold path only (Display/stringify/error/wire/snap).
   Match-arm dispatch, equality, and `?` are pure-int compares (was variant-name string compares /
   `ty==ty && variant==variant`). Native `Ok`/`Err`/`Some`/`None` hold the **reserved** fixed ids
   `VID_OK`(0)/`VID_ERR`(1)/`VID_SOME`(2)/`VID_NONE_VARIANT`(3); user variants follow at `4..`, so the
   reserved range is **disjoint** from every user id. `?`/top-level-error gate on the constants, and the
   native construction path (`alloc_enum`) stamps the constant **directly** (never a `variants[name]`
   lookup) — so a user enum may shadow a native name (`enum Foo: Some(int)`) without a genuine native
   Option/Result ever being stamped with the user's id (was a parity bug: name-resolved native
   construction collapsed identity + broke `?`; fixed 2026-06-16). `Op::NewEnum`/`Op::MatchArm` carry the
   compile-time id; wire/snap carry the dense `variant_id` **directly** (shared `Arc<Program>` ⇒
   meaningful both sides; preserves identity under shadowing). **−20% (1.25×)** on an enum
   construct+match-dispatch micro (`benches/chz/enum.chz`),
   suite-neutral; `Obj::Enum` shrank 56→32 B (Module still caps `Obj` at 88 B). Hands the JIT a numeric
   variant id → constant/jump-table dispatch. See `docs/contributing/benchmarks.md`.
3. **✅ DONE — Closure captures: positional `Vec`, not a per-closure `HashMap`.** Was `Obj::Closure
   { captured: HashMap<String, Value>, .. }` (a `HashMap` ~48 B + string keys **per closure** + a
   string hash on every `GetCaptured`). Now `captured: Vec<Value>` indexed by a compile-time slot;
   `Op::GetCaptured(u32)` is a hash-free `captured[slot]` read; capture names live in
   `Proto.capture_names` (cold path: error fallback + wire/snap name carrying). Nested captures map by
   `CapSrc::Captured(parent_slot)`. **−45% (1.83×)** on a closure construct+capture-read micro
   (`benches/chz/closure.chz`), suite-neutral; `Obj::Closure` shrank 88→64 B (Module still caps `Obj`
   at 88 B). Hands the JIT a constant capture offset. See `docs/contributing/benchmarks.md`.
3b. **✅ DONE — Inline small struct `fields` (no per-struct second malloc).** Was `Obj::Struct {
   fields: Vec<Value> }` — a SEPARATE heap malloc per instance (2M structs = 2M small buffers, ~61MB
   RSS on `benches/chz/many_struct.chz`). Now `fields: Fields`, a hand-rolled enum in `heap.rs`:
   `Inline { len: u8, vals: [Value; 3] }` folds ≤3 fields (the vast majority) into the 64B `Obj` slot —
   **zero second malloc** — while `>3` spill to `Spill(Box<[Value]>)` (exact-length, no `Vec` capacity
   slack). Fields are FIXED at construction (positional hidden-class layout, item 1; no growth sites), so
   inline-or-spill with no capacity is safe. No new dep (no `smallvec`); `Obj` stays 64B
   (`size_of::<Fields>() == 32`, guard-pinned). `Fields` gives a `Vec`-compatible surface
   (`from_vec`/`len`/`as_slice`/`as_mut_slice`/`iter`/`get`/`get_mut`/`heap_bytes` + `Index`/`IndexMut`)
   so the ~16 touch sites + the field-IC hot paths + in-place `s.a = s.b` writes stay byte-identical.
   Mechanical VM-only (checker never names `Obj`); GC `children()` traces every field, `live_bytes`
   counts `Spill` backing only. RSS delta measured post-merge. See `docs/contributing/benchmarks.md`.

**Heap-slot layout (GC-side; principled, low priority — GC moves no bench):**

4. **✅ DONE — Separate the mark bit from the object.** Was `Slot { obj: Option<Obj>, mark: bool }` —
   the `mark: bool` padded the slot to **72 B** (`Option<Obj>` is already 64 B: `Obj`'s spare-discriminant
   niche makes `None` free), so the mark bit cost 8 B of padding on every slot. Now
   `Slot { obj: Option<Obj> }` (exactly 64 B, guard-pinned `size_of::<Slot>() == 64`) + a dense parallel
   `marks: Vec<u64>` bitset on `Heap` (bit `i&63` of word `i>>6`), grown in lockstep with `slots` at the
   new-slot alloc arm. Three one-line helpers `is_marked`/`set_mark`/`clear_mark`; `mark()`/`sweep()`
   rewired to the bitset, EXACT current mark-then-sweep-and-clear protocol (survivors cleared in the
   sweep pass, holes never marked → post-sweep all bits 0). VM/GC-internal, no observable/checker change;
   all GC-stress rooting green. Saves the 8 B mark padding per slot (≈16 MB on the 2M
   `many_struct` bench); the mark test-and-set also touches a compact word rather than a scattered slot
   byte. (Sweep still scans every slot's `obj` — the bitset does not avoid that.) RSS delta measured
   post-merge. See `docs/contributing/benchmarks.md`.
5. **Shrink `Obj` below 64 B.** ✅ DONE for `Module`: boxed to `Box<ModuleData>`, so
   `size_of::<Obj>()` dropped 88→**64 B** (guard, `chzstr.rs:205` / `heap.rs`); `MapData`/`SetData`
   (56 B payload + 8 B discriminant) now cap it. To go below 64 B you'd have to box `MapData`/`SetData`
   too — **but SSO deliberately sized strings to fill the inline `Obj`**, so shrinking un-inlines them.
   Net is a trade-off → measure first.

**Hot-loop access (mostly already addressed or parity-blocked — cross-ref):**

6. **HOF borrow-release clone (new finding).** List `map`/`filter`/`fold` do `self.heap.get(h).clone()`
   to release the heap borrow before `invoke_value(&mut self, …)` — an N×16 B copy per HOF call. A `Vm`
   split (`&mut ExecState` + `&Heap`) lets the borrow coexist. Structural refactor, not a one-session lever.
7. **`for`-loop snapshot (`ListClone`) + per-char alloc** — mandated by the for-loop's observable
   snapshot semantics; `alloc_char` (Phase 3) already halved the string case. Behavior-blocked.
8. **Operand-stack 16 B/Value traffic** → **DONE 2026-07-18: 8B pointer-tag Value shipped** (NaN-box was the wrong scheme; see the dedicated note above) / register VM (#8, low-ROI).

**Land order:** **#1 ✅ → #3 ✅ → #2 ✅ — sequence complete** as **JIT groundwork** (the positional
layouts the JIT codegen wants), each measured against `struct`/`hof`/`enum` (read suite-neutral — they're
dispatch-bound, see caveat — with strong micro deltas: #1 −38%, #3 −45%, #2 −20%).
#4 ✅ done (mark bit → parallel bitset, `Slot` 72→64 B). #5/#6 are principled cleanups, post-JIT. Same discipline throughout: failing-then-green correctness test →
keep the suite green (both worker counts) → measure (`benches/run.chz`) → record the delta in `docs/contributing/benchmarks.md`.

**Highest payoff-per-effort (original M19 batch, all landed):** superinstructions + inline caching +
peephole/const-fold. They attacked dispatch count and name lookup — the two actual costs — without
touching the value model or the GC.

### AtomicInt — lock-free primitive int atomic (LANDED 2026-07-22)

`Atomic[T]` is generic over ANY sendable T (`store`/`exchange`/`cas` hold arbitrary values; `add`/`sub`
numeric only). A lock-free fast path that picks `AtomicI64` backing from the **runtime** init value is
UNSOUND — `x: Any = 5; a := Atomic(x); a.store("hello")` type-checks (`str` <: `Any`) then faults on an
int-only cell, where the old Mutex cell held the string fine. (Tried + REJECTED 2026-07-22 after 2
remediation rounds, branch discarded — the `checker-superset-of-compiler` class: the VM is **type-blind**
at construction, so it can't see the static element type is wider than the runtime int.)

**The fix is a NEW dedicated type, not a patch to `Atomic[T]`.** Add **`AtomicInt`** — statically,
monomorphically int (no `[T]`, nothing to widen), so it can ALWAYS be a lock-free
`std::sync::atomic::AtomicI64`: no runtime type-sniffing, no wider-T hole, the `Atomic[Any]` trap can't
be written. This is the mainstream design — Rust `AtomicI64`, Java `AtomicInteger`, Go `atomic.Int64`,
each distinct from the generic reference cell (`AtomicReference<T>` / `atomic.Value`). `Atomic[T]` stays
exactly as-is (Mutex, general) — **zero regression, purely additive**.

Sketch (additive-native-type pattern, ~3 touchpoints — see the `native-types-first-class-additive-pattern`
memory / `regex.Match` as the template): new `Obj::AtomicInt(AtomicI64)` + ctor `AtomicInt(0)`; checker
registers `AtomicInt` as a reserved `std.concurrency` type with int-only `load/store/add/sub/cas/exchange`
and gates the bare name behind `import std.concurrency`; runtime method dispatch on the new Obj.
**CRITICAL:** `add`/`sub` MUST keep the i64-overflow FAULT via a `compare_exchange` CHECKED CAS-loop —
NOT raw `fetch_add`/`fetch_sub` (they wrap silently = behavior regression). `SeqCst` ordering everywhere
(matches the sequential consistency the Mutex gave). Tests: overflow still faults; a high-contention
`parallel:` counter (N tasks × M `add(1)` == N*M); the suite green. Perf: the discarded generic attempt measured **1.85× under contention**
(uncontended ~flat); NOT on the M19 `fib`/`loop`/`primes` benches → gate on a **contention microbench**,
record in `docs/contributing/benchmarks.md`, and if no measurable win SAY SO.

**LANDED 2026-07-22** (additive, `Atomic[T]` untouched). Shipped exactly per the sketch: unit `Ty::AtomicInt`
+ `Obj::AtomicInt(Arc<AtomicIntCore{v: AtomicI64}>)` + `Op::NewAtomicInt`, mirroring the four reserved
`std.concurrency` names at every checker/VM site; `native struct AtomicInt` (no `[T]`) in
`std/concurrency.chz` harvests the concrete int method table. `add`/`sub` use a checked `compare_exchange`
CAS-loop (keeps the i64-overflow fault, byte-identical `"integer overflow in Add/Sub"`), `SeqCst` on every
op. **Perf: the contention win materialised — ~2.7× faster than Mutex-backed `Atomic` on an 8-way int
counter** (16M adds: 1.73s vs 4.73s median; uncontended a wash), recorded in `docs/contributing/benchmarks.md §AtomicInt`.
Tests: roundtrip, add/sub overflow fault, 8×10000 contention counter == 80000, `import
AtomicInt from std.concurrency` runs (reserved-name hole closed), bare-unlicensed = checker error.

### Post-M19 next levers (ranked — diagnosed 2026-06-12; **status updated 2026-06-13**)

> **Status (2026-06-13):** Tier 1 is DONE — #1 method-IC (Phase 6) and #2 inline-hot-ops (Phase 7)
> landed; #3 `Op::Call` spec was analyzed and **deferred (no-gain after the Phase 7 inline)**. Tier 2 is
> underway — #4 adaptive quickening **v1 (binops) landed**, #5 **index specialization landed**
> (`GetIndex`/`SetIndex` Int-key fast path), and the #4 *CallMethod* extension **landed (2026-06-13):
> N-way polymorphic method-call IC + sticky-deopt + clone-free megamorphic slow path, `poly_method`
> −33% (6.0× → 4.28× CPython)** — this unifies the field/method caches under one adaptive form.
> **Genuinely remaining:** the **denser int-keyed `map`** representation **also landed (2026-06-13,
> `map` 2.68× → 1.94× CPython, −26% on merged HEAD)**, so what's left is the Tier-3 milestones (#6 JIT / #7 8B-Value **DONE** (pointer-tag) / #8 register
> VM). Per-lever tags below; landed details + measured deltas in `PROGRESS.md` "Current focus" and
> `docs/contributing/benchmarks.md`.

The M19 cheap batch + call-flatten + SSO are spent. Latest gap tracks **call density**: `loop` (no
calls) **1.32×**, `primes` 2.53×, `str` 2.65×, `struct` 2.71×, `map` 2.83×, `list` 2.97×, `fib` (all
calls) **3.54×**; startup **0.094× (11× win)**. The bottleneck is **call overhead + per-op dispatch +
a few alloc paths — NOT the value model or the GC** (confirmed: `loop` is already at the match-dispatch
floor; ints are unboxed; GC is share-nothing per-thread and moves no bench). Target is **CPython 3.14**
(specializing adaptive interpreter + optional copy-and-patch JIT), so the interpreter can *narrow* the
gap but a JIT is the only path to *match/beat* it on tight compute.

**Tier 1 — interpreter, cheap→medium, behavior-preserving, each hits a measured bench:**

1. **✅ DONE (Phase 6, 2026-06-13).** **Method-call IC + flatten `do_method_call`** *(hit `struct` −9%)*. `do_method_call`
   (`mod.rs:~3868`) still string-looks-up `def.methods.get(method)` per call **and** still recurses into
   a fresh `run_until` — call-flatten only covered `do_call`'s plain-fn fast path (see its own follow-up
   note). Add a per-call-site monomorphic cache (`tid → proto`, the same shape as the landed `field_ic`)
   and push the method frame in place. Symmetric to the field IC; reuses that machinery.
2. **✅ DONE (Phase 7, 2026-06-13) — landed as "inline hot ops"** *(moved every op-bound bench: `loop` −15%, `list` −17%, `primes` −8%, `fib` −6%)*. The inline-the-hottest-ops sub-lever shipped; the other two below (lazy `span`, the `mn.is_some()` branch split) were left **unshipped** (predictably-false cheap branches, low payoff vs the inline win — revisit only if a profile shows them). **Trim per-op overhead in `run_until`** — three things run
   **every instruction** that are pure overhead on the no-scheduler (`mn.is_none()`) path the benchmarks exercise:
   - `span = proto_ref.lines[ip]` (`mod.rs:2157`) is loaded every op but used **only on fault** → pass
     `(pid, ip)` to the error path and reconstruct the span lazily there.
   - the `if self.mn.is_some()` reduction-count branch (`mod.rs:2137`) + the cancel check (`mod.rs:2122`)
     only matter once a scheduler is installed → split a lean no-scheduler loop body from the
     scheduler-installed body (or hoist them off the `mn.is_none()` back-edge).
   - `self.step(op, span)` is a **separate fn call per opcode** → inline the ~6 hottest ops (GetLocal, the
     superinstrs, Jump, Call, Return) directly in the loop, delegate the long tail to `step`.
3. **⏸️ DEFERRED — no-gain (Phase 8 analysis, 2026-06-13).** **Call-site specialization for `Op::Call`** *(was aimed at `fib`)*. After the Phase 7 inline, `do_call`'s happy path is already lean (the deref a call-IC skips is ~2–3 instrs); fib's residual is frame-setup in `finish_frame`, which a dispatch cache doesn't touch — and a correct call-IC can't avoid a heap-specific callee handle ⇒ `swap_ctx` hazard for ~0 gain. fib's real lever is #4/#6. Each call re-checks Func/Closure/
   Native, derefs the heap callee, and re-validates arity (CPython `CALL_PY_EXACT_ARGS`); full rationale in `docs/contributing/benchmarks.md`.

**Tier 2 — structural, medium→large:**

4. **✅ v1 + CallMethod extension LANDED (2026-06-13).** **Adaptive opcode quickening (PEP 659)** *(the single most CPython-like lever)*. v1 specializes the un-fused generic binop arms (`Add..GtEq`, `Eq`/`NotEq`) to an int/int fast path behind a per-`Vm`, per-site `(proto,ip)` deopt guard (side table `quicken`/`quicken_base`, mirrors `field_ic`/`method_ic` — no `Op`/compiler/interp change ⇒ behavior-preserving by construction). Measured **`primes` −7–8%**. **CallMethod extension (done):** the method-call IC's single `MethodIcCell` is widened to an N-way (4-way) `MethodIcSite` carrying the *same* one-way sticky-deopt discipline — a bounded-megamorphic site (≤4 receiver types) HITS a way per type and flattens; a 5th distinct type latches `sticky` and goes slow (now clone-free: borrows `Arc<Program>.structs` instead of cloning the whole `StructDef`). This **unifies** the field+method caches under one adaptive form (`GetIndex` is already covered by #5). Measured **`poly_method` −33% (6.0× → 4.28× CPython)** on a new megamorphic bench; side table still int-only (no `GcRef`) ⇒ behavior-preserving by construction. After an op runs once, rewrite-in-place to a type-specialized form behind a deopt guard. **Constraint
   (same one P2b/P4 hit):** bytecode is shared `Arc<Program>` read-only across `--parallel` workers, so
   quickened cells must live in a per-`Vm` side table keyed by site, not mutate the `Op`.
5. **✅ DONE (2026-06-12).** **map/list index specialization** *(`list` −4%; `map` neutral — it's FxHashMap-probe-bound, not dispatch-bound, so the predicted `map` win needs a **denser int-keyed map** representation, a separate lever, not this tweak)*. `GetIndex`/`SetIndex`
   got an Int-key fast path (skips `hash_key_rooted` rooting) + inline dispatch in the `run_until` hot arm; 7 `idxspec_*` regression guards.

**Tier 3 — big, separate milestones:**

6. **Cranelift method-JIT** — the only path to *match/beat* CPython 3.14 on compute. Counter-triggered,
   JIT the hot protos (Python's tier-2 model). End-game; only once the language is fully frozen. #4 is the
   lower-risk stepping stone toward it.
7. **8B `Value` — DONE 2026-07-18** via int-favoring pointer-tag (NOT NaN-box, which stays blocked by full i64). `loop`→1.03× CPython, `fib`→2.95×; see the dedicated note + `docs/contributing/benchmarks.md`.
8. **Register VM / generational+incremental GC — low ROI** (dispatch is already near the match floor; GC
   moves no bench). Deprioritized; revisit only if a real workload proves otherwise.

**Sequencing (updated 2026-06-13):** Tier 1 is **done** (#1, #2 landed; #3 deferred), and Tier 2 is
**done** — #4 (v1 binops **and** the `CallMethod` N-way extension) + #5 (index spec **and** the denser
int-keyed `map`) all landed. With both the `CallMethod` adaptive quickening and the denser `map`
shipped, the high-ceiling play left is **#6 (Cranelift method-JIT)** as the JIT end-game (#7 8B-Value shipped 2026-07-18 via pointer-tag; NaN-box stays
blocked; #8 register VM / gen-GC stays low-ROI). All steps: behavior-preserving, golden-clean,
measure-first, each targeting a named bench.

**M19 Phase 1 done (2026-06-11):** peephole/const-fold + superinstructions + `invoke_value` clone
kill — all behavior-preserving (1516 tests + the full suite green). Results in
`docs/contributing/benchmarks.md`.

**M19 Phase 2 done (2026-06-11):** in-place call args in `do_call` (per-call `Vec` gone, `fib` −13%)
+ `stringify`-into-buffer for `BuildStr` (`str` −5%) — both behavior-preserving (1518 tests + the full
suite green, 4-agent S++ panel clean). Results in `docs/contributing/benchmarks.md`. Remaining `str`
lever is `ConstStr` interning; the next dispatch win is inline caching (Phase 2b, below).

### M19 Phase 2b — inline caching via global-slotting (✅ landed 2026-06-11)

Landed as designed below. Net: `fib` −9%; other microbenches flat (their hot loops are local-bound).
Implementation notes vs the plan: `Module` became `{ slots: Vec<Value>, index: HashMap<Box<str>,u32> }`
(the `index` is kept alongside the `Vec`, not discarded — it backs `module.member`/imports/native
population, and reverse-iterating it in slot order via `module_slot_pairs` is how the snapshot stays
deterministic); the old name-keyed ops were *replaced*, not kept. The historical design note follows.

The next dispatch win, deliberately split out from Phase 2 because it is **not** a local opcode
tweak. Today `Op::GetGlobal(String)` does a `HashMap<String,Value>` probe by name every read
(`mod.rs` `module_global`). A monomorphic IC would cache the resolved location, but two facts block
the naive "cache in the opcode" approach:

1. **Bytecode is shared read-only across threads.** Under `--parallel` the `Program` is an
   `Arc<Program>` and every worker fiber reads the *same* `Op` slices — so an opcode cannot carry a
   per-site mutable cache cell without synchronization.
2. **Globals are name-keyed, not slotted.** `Obj::Module { globals: HashMap<String,Value> }` has no
   stable index to cache.

**Plan:** resolve globals to slots at compile time. The compiler assigns each module global a stable
`u32` slot and emits `GetGlobalSlot(u32)` / `SetGlobalSlot(u32)` / `DefineGlobalSlot(u32)`;
`Module.globals` becomes a `Vec<Value>` (name→slot map kept only for `module.member` field reads and
error messages). The read becomes a bounds-checked `Vec` index — no hashing, no string.

**The concurrency constraint (the reason it is its own milestone):** the lazy module-fault path
(`ensure_module_faulted` / `fault_module` / the worker module snapshot) reconstructs a worker's home
module on first access. Slot order must be **identical** between the parent's compiled module and any
faulted worker copy, or a worker reads the wrong global. The snapshot (`to_snap`/`replay_snap`) and
`ModuleInline`/`ModuleAlias` replay must round-trip slots, not names. This needs its own dedicated
correctness pass + the `--parallel` module-fault tests, so it is scheduled separately rather than bundled
with the Phase 2 allocation kills.
