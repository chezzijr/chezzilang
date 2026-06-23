# Chezzi — Language Gaps

Limitations found writing real programs against both engines. **Open** entries carry what they block +
a fix sketch with `file:line`. Resolved gaps collapse to a one-line log — full detail in `PROGRESS.md`
+ cited `examples/*.chz`.

Legend: 🔴 blocks apps · 🟡 friction · ⚪ latent (unreachable) · 🟢 works.

Last updated: 2026-06-23.

> Core constructs all in place (language **still evolves** — features land via own milestone; M19 is
> pre-JIT perf, not a freeze). This doc = unified actionable backlog (open language/stdlib gaps + M19
> perf track). Speculative tracks in [`docs/future.md`](docs/future.md); perf numbers in
> [`docs/benchmarks.md`](docs/benchmarks.md).

---

## Open gaps — START HERE

### 🔴 Soundness / correctness nits

- **`INT_MIN` is unwritable as a literal** — `-9223372036854775808` lexes as unary-minus over
  `i64::MAX` → "number too large", same as Rust. (The companion soundness nit — left-shift wrapping
  silently to `INT_MIN` — is **resolved**: `<<` now overflow-checks like `+ - * / %` and raises a
  recoverable `integer overflow in Shl`; only round-trip-safe shifts incl. `-1 << 63 == INT_MIN`
  still succeed.)

### 🟡 Type-system + runtime depth (latent — unreachable on HEAD)

- **⚪ Checker `=` to by-value captured local** (verified 2026-06-16). `infer_closure` pushes no capture
  floor (`checker/mod.rs:2841`); an inner fn/closure writing an enclosing local would type-check then
  misroute to `SetGlobalSlot` (`compiler/mod.rs:932`, no `SetCaptured` arm). No surface syntax reaches
  it (closures = single expr; nested named fn rejected; `spawn:`/`defer:` gated). Re-open if block-bodied
  closures / nested-fn names land. Fix: push capture floor in `infer_closure` → `cannot reassign captured
  binding` error.
- **⚪ Module-scoped type error strings leak the qualified key** (verified 2026-06-20). A few defensive
  errors interpolate raw `mod::Point` instead of bare display (`vm/mod.rs:12121`, `interp/mod.rs:4636/
  4833/5003` + VM twins). No well-typed program reaches them (checker gates index/slice/iter on protocol
  method). Strings are byte-identical with interp twins (parity-tested) → fix must touch all sites at
  once. Fix: wrap key in `crate::compiler::bare_display(...)` in lockstep.

### 🟡 Stdlib breadth (low priority — library fill)

Current: `fs`/`io`/`os`/`process`/`time`/`request`/`regex`/`json`/`math`/`cmp`/`str`/`iter`/`cancel`/
`ref`/`net`. Gaps block write-heavy automation, randomness, crypto, CLI. Ranked by leverage.

**Native (Rust):**
- **`std.rand`** *(highest)* — no RNG. Unblocks shuffle/sample/test-data/sims/games. OS entropy → seedable PRNG.
- **fs mutations** — read-only today; missing `mkdir`/`remove`/`rename`/`copy`/`append`.
- **Encoding/crypto** — no base64, hex, sha/md5, uuid, url-encode.
- **`std.process` polish** — only `cmd(line)` via `sh -c` (injection-prone), stdout discarded on failure.
  Want structured result (both streams + exit code) + args-array form.
- **`std.request` nit** — remaining: per-call timeout override + query (`?k=v`) builder (timeouts hardcoded).

**Pure-Chezzi `std/*.chz` now (dogfood):** path ops (`join`/`basename`/`dirname`/`ext`/`normalize`),
`argparse`, CSV, duration/date decomposition, data structures (heap/PQ, deque, counter, ordered map).

**Language-level:** `i64`-only (no `byte`/`u8` scalar). `bytes`/`bytearray`/str↔bytes/`list()`/`set()`/
`map()` ctors/`Iterable[T]`+`.iter()` all SHIPPED. Remaining: no `byte`/`u8` scalar, no non-UTF-8 codecs
(latin1/utf16), no `tuple()`/`bool()` ctors. `bignum` stays a non-goal. **`yield`/generators SHIPPED
(VM-only)** — `fn -> Iterator[T]` may `yield`; see resolved log + `examples/generators_basic.chz`.

### 🟡 Surface ergonomics / DX (fresh-dev audit, 2026-06-23)

Found probing the language cold as a Python/Go/Rust dev. None block apps (workarounds exist), but each
is a predictable first-hour stumble. Ranked by friction.

1. **Method vs free-function split-brain** *(highest friction)*. The same conceptual op is a receiver
   method in one place and an import-required free fn in another, with no predictable rule:
   - **str:** `upper`/`lower`/`trim`/`split`/`join`/`starts_with` are **methods**, but `ends_with`/
     `replace`/`repeat`/`reverse`/`pad_left`/`index_of`/`count`/`strip_prefix`/`strip_suffix`/`split_lines`
     are **`std.str` free fns** — so `s.starts_with(x)` works yet `s.ends_with(x)` is a type error.
   - **list/iter:** `map`/`filter`/`fold`/`sum`/`sort`/`contains`/`index_of`/`concat` are **methods**, but
     `enumerate`/`zip`/`any`/`all`/`find`/`flatten`/`reduce`/`take`/`drop` are **`std.iter` free fns**;
     `min`/`max`/`clamp` are **`std.cmp` free fns**.
   - A dev can't guess which surface an op lives on. Fix: (a) re-export the common `std.iter`/`std.str`/
     `std.cmp` ops as receiver methods (thin forwarders in the checker method table), or (b) document a
     hard rule + add the obvious missing methods. Lowest-effort high-value: add `ends_with`/`replace`/
     `strip`/`find` as **str methods** to kill the worst asymmetry.
2. **Chained `else if` rejected in expression-`if`** (`parser/mod.rs:1034`). Statement chains fine; the
   expression form does not — `a := if p: 1 else if q: 2 else: 3` → "expected ':', found 'if'". Must nest
   with parens. Fix: in `parse_if_expr`, after consuming `Else`, if next token is `If` recurse into
   `parse_if_expr` for the else-branch instead of `expect(Colon)` (~3 lines).
3. **No collection operators.** `[1,2] + [3,4]` (concat), `[0] * 3` (repeat-init), set `| & - ^` are all
   type errors (`checker/mod.rs:5276/5337`). Functionality exists via methods (`.concat`, `.union`/
   `.difference`) but operator forms are reflexive for Python/Rust devs; set algebra reads badly as
   `.union(.difference(...))`. Fix: desugar list `+`/`*` and set `|&-^` to existing methods in the binop arms.
4. **No stepped / reverse range.** `for i in 10..0` yields nothing (no auto-reverse); `range()` takes only
   `(end)`/`(start,end)` — no step; a range isn't sliceable (`(0..10)[::2]` errors). Counting down/by-N
   forces a manual `while`. Slices already do `::step`; ranges should mirror it (3-arg `range` and/or
   reverse on `..`).
5. **No `print` newline/sep control.** `print(a, end="")` → "named arguments not supported on builtins"
   (`checker/mod.rs:6274`); `std.io` has no `write`; no `println`/bare split. Incremental output is
   impossible without a trailing newline. Fix: add native `std.io.write(s)` (no newline) or special-case
   `end=`/`sep=` on `print`.
6. **`assert` takes no message.** `assert(x==y, "msg")` → type error (args parse as a tuple); failure
   prints only `assertion failed`, no context. Fix: accept optional 2nd `str` arg in the `assert`
   checker special-case + thread into the panic message.
7. **No safe numeric parse.** `int("abc")`/`float("x")` raise a (recoverable) panic — no
   `str.to_int() -> int?` / `int?(s)`. Untrusted input must be wrapped in `recover:`. Fix: add
   `to_int`/`to_float` str methods returning `Option`.

**Minor / noted:** no `map.items()` (have `.keys()`/`.values()` + `for k,v`); no `type()`/`typeof`; no
`input()` (have `std.io.read_line`); no chained comparison (`1<2<3`); `json.parse` widens all numbers to
float (int-ness lost). `**`/`//` absence is by-design (no base operator).

**Docs clarity (not a bug):** `/` is integer division and `%`/`/` truncate toward zero (`-7%3 == -1`,
`-7/2 == -3`) — correct-as-designed (see Verified working) but **conflicts with the "Python-feel"
branding** a newcomer reads (`7/2==3.5`, `-7%3==2` in Python). Add a loud one-liner in `syntax.md` §4 so
the surprise is caught at read-time, not via a silent wrong result.

### Tier 4 — ecosystem (toolchain)
REPL, formatter, LSP, package manager/registry, debugger, doc comments + docgen.

### ⚙️ Performance + runtime backlog (M19 — detail in [`docs/future.md` §4] + [`docs/benchmarks.md`])

M19 = pre-JIT perf, not a freeze. Every item here is **behavior-preserving + two-engine parity** (a VM
speedup that diverges from interp is a bug). Gap to CPython 3.14: **~1.3×–3.5×** (worst `fib` 3.54×;
`loop` 1.32× at dispatch floor), startup ~11× faster. Discipline: failing-then-green parity test → keep
parity → measure `benches/run.chz` → record delta in `docs/benchmarks.md`. **Landed** (don't re-flag):
peephole/const-fold, superinstructions, global-slotting, `ConstStr` interning, struct-field IC, FxHash,
SSO, method-call IC, inline-hot-ops, adaptive quickening (PEP 659), map/list-index specialization,
positional struct/enum/closure layout (memory levers #1/#2/#3).

> **Sequence by JIT-coupling, not payoff.** Cranelift method-JIT hardcodes value repr, memory layout,
> calling convention, opcode set, GC invariant. Rank by lock-in cost.
> - **Tier A — MUST precede JIT:** (1) ✅ positional struct/enum/closure layout (memory levers — ALL
>   LANDED). (2) **GC invariant** — gen/incremental GC needs write barriers + safepoint placement baked
>   into codegen; lock the GC contract pre-codegen. (3) ✅ new `Value`/`Obj` variants (`bytes`/`bytearray`
>   /`Iter` cursor — LANDED, within 88B `Obj` cap). (4) **NaN-box** — highest coupling but ⛔ blocked by
>   full i64; pin as "if i64 revisited, MUST be pre-JIT."
> - **Tier B — JIT-neutral:** stdlib breadth, string concat/split builder, ecosystem/tooling. (`ref T`
>   sugar LANDED.)
> - **Tier C — superseded:** register VM (JIT supersedes), mooted concurrency, unreachable latents.

- **🟡 Memory-layout levers** (`docs/future.md` §4). Levers #1/#2/#3 LANDED. Remaining:
  4. **GC mark-bit bitvec** — `Slot{obj,mark:bool}` (`heap.rs:234`) interleaves 1B in 88B; packed bitvec
     = dense sweep. Only if GC becomes hot (post-JIT).
  5. **Shrink `Obj` <88B** — guard `chzstr.rs:205`; box rare big variants. Trades against SSO — measure first.
  6. **HOF borrow-release clone** — `map`/`filter`/`fold` clone list to release heap borrow before
     `invoke_value`. Fix: `Vm` split (`&mut ExecState` + `&Heap`). Structural refactor.
  7. **`for`-loop snapshot (`ListClone`) + per-char alloc** — parity-blocked by interp snapshot semantics.
  8. **Operand-stack 16B/Value traffic** → NaN-box (blocked) / register VM (low-ROI).

- **🔵 End-game tracks** (only once language stopped moving):
  - **Cranelift method-JIT** (`future.md` §4 #6) — only path to match/beat CPython 3.14; whole backend.
  - **NaN-box `Value` (16B→8B) — ⛔ BLOCKED by full i64** (`value.rs:18`): i64 + tag don't fit in 8B.
  - **Register VM** — low ROI (dispatch near match floor).
  - **Gen/incremental GC** — low ROI (GC moves no bench).
  - **String concat/split builder** — medium; `join` already buffers (`vm/mod.rs:7402`), `+`/`split` unbenched.

### 🟠 Deferred — real work, lower urgency

- **FFI surface expansion** (`std.cffi`, `src/native/cffi.rs`; spec.md §FFI, syntax.md:1232) — v1 =
  scalars only. Deferred: structs-by-value, callbacks/fn-pointers, varargs, opaque pointers/userdata,
  `char*` ownership/`free`. (Sync same-thread scalar-by-value callbacks landed — see git.)
- **Graceful shutdown of accept loops** + per-connection handler→acceptor signal channel
  (`concurrency-tier-d.md:297`).
- **Reduction-constant tuning** (D3) — pick `CONTEXT_REDS` + per-op vs per-back-edge accounting
  (`concurrency-tier-d.md:363`).

### ⚫ By-design non-goals (revisit only if it bites)

- **`Shared.update` same-box hold-and-wait** — `update(f)` holds `update_lock` across `f`, so a `recv`
  needing the SAME box deadlocks. Universal hold-and-wait class (Go #13759, Rust
  `clippy::await_holding_lock`, BEAM no shared locks). Rule: don't block on a value needing the same box.
  `update` stays (only atomic RMW; dropping it reintroduces lost-update race). Future: maybe a `share`
  binding modifier + lint. (`concurrency-tier-d.md:245`).
- **Concurrency BEAM-flavored:** priority classes, restart/supervision (out of scope C5); contended
  shared channel (concurrent-divergent by design), inline-body blocking recv, eager-nursery cross-wake.
- **`int32`/unsigned C ints** — no such scalar (FFI widens at boundary).
- **Defaults/named args on built-in methods** (`map`/`push`/`len`) — by design (syntax.md:216); user
  methods/fns/ctors have them.

---

## 🗑️ Deprecated-engine (`--interp` / `--serial`) — WON'T-FIX pending removal

Both cooperative engines are slated for removal, leaving M:N `--parallel` as sole engine. Items below are
coop-only limits M:N already handles → don't invest in a coop fix. **Tradeoff:** `--interp`/`--serial`
are the parity oracles (differential testing caught most resolved bugs); removing them ends that net.
`--serial` is cheap to keep (shares VM compiler/opcodes) — consider dropping only `--interp`, give
`--serial` the D3 back-edge yield budget to keep oracle + close CPU-preempt.

- **Cross-nursery coop flatten.** Resolved under `--parallel`
  (`examples/parallel_cross_nursery_circular.chz`); coop can't flatten an outer-nursery fiber woken by an
  inner one (`concurrency-tier-d.md:342`).
- **Coop + `--interp` can't preempt CPU-bound tasks** — switch only at yield points. Pure-CPU loop
  monopolizes the thread → sibling canceller never runs. 2e9-iter worker under cancel aborts ~0.5s on
  `--parallel`, runs ~69s on coop (`examples/parallel_cancel.chz` carries no golden). M:N preempts (`vm:2871`).
- **`--serial` generator airlock** — `--serial` runs a generator across the spawn airlock (parent/task
  share one mutable generator); default OS-thread engine correctly rejects. `--serial` going away.

---

## 🟢 Verified working (so we don't re-flag)

- Struct equality (structural), string indexing/ops, list-of-structs + nested-list read, by-ref list
  sharing across calls.
- `if`/`match` as expressions (incl. interpolation), `Result`/`Option` + `?`, exhaustive-match, deep
  recursion, integer `+ - * / %` **and `<<`** overflow → recoverable panic,
  int-div truncation, `%` on negatives.
- **Fuzz-pass robustness** (2026-06-21, both engines): parser nesting-depth guard, structural-depth
  guard (self-referential cycle), call-depth cap (10000), index/slice OOB + Python-asymmetric clamp,
  `slice step 0` fault, generator idempotent-`None`, per-iteration loop-var capture, deep-copy
  spawn/parallel airlock, default-arg expressions fully checked, generic bound enforcement, multibyte
  `str` by codepoint, unicode identifiers, empty file, `bytearray` byte-range fault, `assert` fault.
- All std modules on both engines, recursive/self-referential structs GC-clean, mutable `self`,
  nested-list DP, empty-map K,V inference from later use.
- User-struct iterator protocol (`next() -> Option[T]`, lazy, early-break-safe) + `Iterator[T]` bound +
  user parameterized protocols; lazy adapters compose without `yield`.
- Python-colon slicing `xs[a:b:c]` (open bounds/step/reverse/negative, as assign target),
  comprehensions (list/set/dict + guard), tuple-destructuring `for` + `enumerate`/`zip`, optional
  chaining `x?.f` + `??`, declared-non-void fn must-return-on-every-path (Option B; inline-expr bodies
  implicitly return).

---

## Resolved log (one line each — full detail in `PROGRESS.md` + cited examples)

**Soundness holes (2026-06-21/22 fuzz pass) ✅** · string-interpolation checker bypass (shared
`interpolation` parser, `check_interpolation`) · empty-collection `Ty::Unknown` → refine-on-first-use +
insertion-site Hashable check + PERSISTENT scope-wide first-use pinning (residuals: simple-var-receiver
only, side-effect pushes in sibling expression-arms) · `Ty::Unknown` family: recursive/forward-ref
return → fixpoint inference (`infer_returns`), generic-nullary-variant (`Box.Empty`/`None`) → same
refine pass · import name-collision rejection (`bind_import`/`import_binds`) · duplicate-binding-in-
pattern → compile error (`first_duplicate_binder`) · `ref` shared-method-name expr-receiver false
rejection (desugar `receiver_struct_ty`/`fn_ret_struct`) · `list.map`/`.filter`/`.fold` shrink-callback
panic → rooted snapshot · two Rust panics fixed; concurrency core fuzzed clean.

**DX / diagnostics ✅** · infinite-recursion stack trace bounded (2026-06-23) — `format_trace`
(both engines, byte-identical) now collapses consecutive same-name frames to `… (× N more identical
frames) …` + caps the collapsed list to head 10 / tail 10 with a `… (M frames elided) …` marker, so a
recursion fault prints ~4 lines instead of ~10_001 (`vm/mod.rs`/`interp/mod.rs` `format_trace`).

**Type-system/runtime ✅** · one-way C-like `int`→`float` widening (2026-06-22) — runtime
`Op::CoerceFloat`/`coerce_float` at every value-def sink (typed `let`, fn/method/closure args incl. int
var via prologue, float param defaults, `-> float` returns, float struct fields, native/extern float
params, float/all-literal collection literals); anti-lossy `float`→`int` stays a type error; carve-outs:
un-annotated non-literal mixed collection, plain reassign to float local, generic `T`, compound/nested
float annotation · bare-fn misdiagnosis → real fix = missing-return check (Option B) + inline-expr body
implicit return + nil-in-value-position rejected · non-const default exprs · calling `fn`-typed field ·
`sort_by_key` · `Ref[T]` box · stack traces · overflow policy · loop-var reassign rejected ·
break/continue in spawn/defer nested in loop rejected (2026-06-16) · break/continue out of `parallel:`
in loop → `Op::ReclaimNursery` (`d8fc2b4`).

**Scripting ✅** · comprehensions (`481514b`) + nested clauses (`auto-task/comprehension-nested-clauses`)
+ stateful-iter laziness · Python-colon slicing (`src/slice.rs`) · `Iterator[T]` (M13) · `os.exit` ·
list `.concat`/`.extend` + map `.merge`/`.update` · hex/bin/oct literals · tuple-`for` + `enumerate`/
`zip` · optional-chaining + `??` · `defer` block-scoped + `defer:` form · `ref T` transparent ref
bindings (auto-deref, lowers to `Ref[T]`, parser/checker/desugar only; non-sendable) · **`yield`/
generators** (VM-only; `fn -> Iterator[T]` may `yield`, call returns a suspendable coroutine driven by
`.next()`; interp rejects `yield` so two-engine parity waived; `examples/generators_basic.chz`).

**Concurrency ✅** · pending-spawn drop on early `parallel:` escape · VM nursery-leak on `?`/return
escape · Path C recv-in-native-callback thread-demotion (`f828ef7`, D complete) · `std.cancel`
Token/timeout + tree propagation (`Token.derive()`, transitive cancel/`done()`, `examples/cancel_tree`) ·
cross-nursery M:N flat scheduler · `Channel.close()` · per-socket `timeout_ms` (D6c) · per-connection spawn.

**Memory-layout levers ✅** · #1 positional struct layout (flat `Vec<Value>`, names in `StructDef`;
2026-06-16) · #2 enum `variant_id: u32` (`Program::variants_by_id`, pure-int dispatch/eq/`?`; native
`Ok`/`Err`/`Some`/`None` reserved ids; −20%, `Obj::Enum` 56→32B) · #3 closure captures positional
(`Vec<Value>` + `GetCaptured(u32)`, names in `Proto.capture_names`; −45%, `Obj::Closure` 88→64B).
All three: behavior-preserving + parity, JIT groundwork (constant offsets).

**FFI ✅** · sync same-thread scalar-by-value C callbacks (callbacks #4 sync subset) + panic-safe
`native_reentry` guard + defer callback host-ptr capture.

**Rounds/Milestones ✅** · R1 (#1–9) index/field assign, HOF params, list methods, match, tuples · R2
(#10–15) `ord`/`chr`, `sort_by`, `std.cmp`, bitwise, map iteration, nested/tuple patterns · M7 generics ·
M8/M9 stdlib (`json`/`time`/`fs`/`process`/`set`/`regex`/`request`) · M10 type-depth · M11 robustness
(`recover:`, `T!`, iterator protocol, match guards/ranges, default/named args) · M14 generics-depth ·
M15 slicing + Index/IndexSet/Slice · M20 in-language tests (`assert`/`test fn`/`chezzi test`) · M21
nominal `newtype` (`31f2f85`) · enum methods (`008444f`) · raw strings (`7a645b8`) · `extern "lib"`
(`15e7818`) · module-scoped types + qualified match patterns (`e269f16`) · CLI `init`/`docs`/
`module:function` entrypoint, `--interp` flag dropped (`7a8cc2e`) · std.math fill (`5a25a5c`) ·
std.request verbs.
