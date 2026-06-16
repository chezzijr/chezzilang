# Chezzi — Language Gaps

Known limitations found by writing real programs against both engines (`chezzi run` / `--interp`).
**Open** entries carry what they block + a fix sketch with `file:line` anchors. Resolved gaps collapse
to a one-line log — full detail lives in `PROGRESS.md` + the cited `examples/*.chz`.

Legend: 🔴 blocks real apps · 🟡 notable friction · ⚪ latent (not currently reachable) · 🟢 works.

Last updated: 2026-06-16. Baseline: post-M20 (`assert`/`test fn`/`chezzi test`), Python-colon slicing,
std.math trig + request verbs landed; concurrency D6 complete (Path C resolved). Gaps pass II.

> **Core language is feature-complete:** scalars, `list`/`map`/`set`/`tuple`, generic structs + enums,
> `Result`/`Option` + `?`, generics + structural protocols
> (`Comparable`/`Add`/`Sub`/`Mul`/`Hashable`/`Stringable`/`Error`/`Iterator[T]`/`Index[K,V]`/`IndexSet[K,V]`/`Slice[R]`),
> exhaustive `match` (literals/wildcard/nested/tuple/guards/ranges), closures/HOF, methods, modules, GC,
> two backends, interpolation, pipe, `recover:`, `defer` (block-scoped), default + named args,
> comprehensions, optional-chaining/`??`. What remains is **stdlib breadth + a few runtime-depth nits**.
>
> **This doc is the unified actionable backlog** — open language/stdlib gaps *and* the M19 perf +
> runtime track (memory layout, JIT, GC, NaN-box) with `file:line` anchors. Full design detail +
> the purely-speculative tracks (BEAM shared-nothing concurrency, far-out ideas) live in
> [`docs/future.md`](docs/future.md); live perf numbers in [`docs/benchmarks.md`](docs/benchmarks.md).

---

## Open gaps — START HERE

### 🟡 Type-system + runtime depth

- **⚪ Checker `=` to a by-value captured local — latent, UNREACHABLE on HEAD** (verified 2026-06-16).
  `infer_closure` pushes no capture floor (`checker/mod.rs:2841`); an inner fn/closure writing an
  enclosing local *would* type-check then misroute the store to `SetGlobalSlot` (`emit_store`,
  `compiler/mod.rs:932` — no `SetCaptured` arm). But no surface syntax reaches it: closures parse a
  single expression (assignment is a statement), nested named `fn` is rejected (`unknown name`), and
  `spawn:`/`defer:` blocks are already gated (`capture_floors`). Re-open only if block-bodied closures
  or nested-fn names land. **Pre-emptive fix:** push a capture floor in `infer_closure` → turn the
  silent misroute into the documented `cannot reassign captured binding` error (`docs/syntax.md:828`
  promises read-only captures, enforced today only across `spawn`/`submit`).

### 🟡 Concurrency (deeper tracking in `docs/concurrency.md` §11 + `docs/concurrency-tier-d.md`)

The `--parallel` engine is a true M:N scheduler through D6 (fibers, work-stealing, dirty pool,
netpoller + `std.net`). Remaining items are correctness/semantics nits, not missing breadth.

> **Planned engine consolidation (mooting note).** The intent is to **remove `--interp` (frozen
> tree-walk) and `--serial` (cooperative single-thread VM) later, leaving M:N `--parallel` as the sole
> engine.** Both items marked **[mooted-by-removal]** below are *cooperative-engine-only* limitations
> that M:N already handles — once the cooperative engines are gone they aren't gaps, so they're
> effectively **WON'T FIX pending consolidation** (don't invest in a coop fix). **Tradeoff to weigh
> first:** `--interp`/`--serial` are the **parity oracles** — the "two engines asserted equal"
> differential testing that caught most of the resolved-log bugs. Removing the oracle ends that net.
> `--serial` is the cheap one to keep (shares the VM compiler/opcodes, still byte-identical); consider
> dropping only `--interp` and giving `--serial` the D3 back-edge yield budget so it keeps the oracle
> *and* closes CPU-preempt.

- **🟡 [mooted-by-removal] Cross-nursery wakeups — cooperative-engine flatten pending.** RESOLVED under
  `--parallel` (M:N flat scheduler — multi-level nesting + late-spawn,
  `examples/parallel_cross_nursery_circular.chz`); the **cooperative** engine still can't flatten a fiber
  in an outer nursery woken by an inner one (`docs/concurrency-tier-d.md:342`). Output correct under
  `--parallel`; the gap is coop-only liveness → vanishes when the cooperative engines are removed.
- **🟡 [mooted-by-removal] Cooperative + `--interp` engines cannot preempt CPU-bound tasks.** They
  switch fibers only at yield points (channel ops, blocking `recv`, back-edge-to-scheduler). A pure-CPU
  loop with no channel op monopolizes the single thread → a sibling canceller/timeout never runs.
  **Same source diverges by engine:** a 2e9-iter CPU worker under manual-cancel aborts mid-flight on
  `--parallel` (~0.5s) but runs to completion on coop/`--interp` (~69s). This is why cancellation
  examples carry no golden `.expected` (`examples/parallel_cancel.chz`) — single-engine would unblock
  goldens. **M:N already preempts** (reduction-counting, D3, `vm:2871`), so removing the cooperative
  engines closes this for free; the only coop-side fix worth considering is the back-edge yield budget
  *if* `--serial` is kept as the oracle.
- **🟡 `Shared.update` same-box hold-and-wait — WON'T FIX by design.** Path C resolved the *general*
  case (a `recv` inside `update(f)` waiting on a **different** box thread-demotes + resumes). The
  carve-out: `update(f)` holds `update_lock` across `f`, so a `recv` needing the **same** box deadlocks
  any sender for it. The universal hold-and-wait class (Go #13759 global-only; Rust
  `clippy::await_holding_lock`; BEAM no shared locks). Rule: don't block on a value needing the same
  `Shared` box. Future: a lint when the tooling track lands.
- **🟡 (residual) `break`/`continue` out of `parallel:` inside a loop reclaims its nursery only at fn
  return**, not block-scoped like interp's `exec_parallel` pop — bounded to frame lifetime, output always
  correct. Closing it needs loop-exit-jump codegen to emit a nursery-drain across a nursery boundary.

### 🟡 Stdlib breadth (low priority — language is feature-complete; this is library fill)

Current stdlib (`std.fs`/`io`/`os`/`process`/`time`/`request`/`regex`/`json`/`math`/`cmp`/`str`/`iter`/
`cancel`/`ref`/`net`) covers read-and-transform scripting well. Gaps below block write-heavy
automation, randomness, binary/crypto, CLI tooling. Ranked by leverage.

**Must be native (Rust):**
- **`std.rand`** *(highest leverage)* — no RNG anywhere. Unblocks shuffle/sample/test-data/tokens/sims/
  games. Small: OS entropy → seedable PRNG.
- **fs mutations** — `std.fs` is read-only (`exists`/`is_dir`/`is_file`/`list_dir`/`size`/`glob` +
  `io.read_file`/`write_file`). Missing `mkdir`/`remove`/`rename`/`copy`/`append`.
- **Encoding / crypto** — no base64, hex, sha/md5, uuid, url-encode.
- **`std.process` polish** — only `cmd(line)` via `sh -c` (injection-prone), `Ok(stdout)`/`Err(stderr)`,
  stdout discarded on failure. Want a structured result (both streams + exit code) + an args-array form.
- **`std.request` nit** — verbs + custom headers done; remaining: per-call timeout override + query
  (`?k=v`) builder (timeouts hardcoded 10s/30s/30s).

**Writable as pure-Chezzi `std/*.chz` now (good dogfood / bootstrap probes):**
- **path ops** (`join`/`basename`/`dirname`/`ext`/`normalize`), **`argparse`** (raw `os.args` only),
  **CSV**, **duration / date decomposition** (timestamps only — no y/m/d/parse/duration math),
  **data structures** (heap/priority-queue, deque, counter, ordered map).

**Language-level:** `i64`-only + no `byte` type blocks clean binary/buffer work (future `bytes`
*sequence* type, not a new scalar — Python model). `bignum` and `yield`/generators are non-goals.

### Tier 4 — ecosystem (toolchain, not the language)
REPL (huge for scripting iteration), formatter, LSP, package manager / registry, debugger, doc
comments + docgen. (`assert` + test runner shipped M20.)

### ⚙️ Performance + runtime backlog (M19 perf track — detail in [`docs/future.md` §4](docs/future.md) + [`docs/benchmarks.md`](docs/benchmarks.md))

Language is **frozen feature-wise**; M19 is pure optimization, so every item here is
**behavior-preserving + two-engine parity** (a VM speedup that diverges from the interp is a bug).
Current gap to CPython 3.14: **~1.3×–3.5×** slower (worst on call-bound `fib` 3.54×; `loop` 1.32× is at
the dispatch floor), startup ~11× **faster**. Discipline per item: failing-then-green parity test →
keep parity → measure `benches/run.chz` → record the delta in `docs/benchmarks.md`. **Already landed**
(don't re-flag): peephole/const-fold, superinstructions, global-slotting, `ConstStr` interning,
struct-field IC, FxHash, SSO, method-call IC, inline-hot-ops, adaptive quickening (PEP 659),
map/list-index specialization — all in `docs/benchmarks.md`.

- **🟡 Memory layout & access levers** *(diagnosed 2026-06-16, `7e4fc42`; `docs/future.md` §4 "Memory
  layout & access patterns")* — **caveat: bench is dispatch/call/alloc-bound, NOT layout, so these read
  mostly neutral as speedups; their real value is JIT groundwork** (positional layouts → constant
  offsets the JIT codegen needs). Land order **#1 → #3 → #2**.
  1. **Shared per-type struct layout** (hidden-class/`__slots__`) — `Obj::Struct` (`heap.rs:162`) stores
     type name + every field name as `Box<str>` *per instance*, both static in `StructDef` (`op.rs:378`).
     → `fields: Vec<Value>` positional, names by `tid` on cold path. Kills N+1 allocs/inst + the
     `==`-name-clone (`mod.rs:4483`). **Biggest redundancy; land first.**
  2. **Enum `variant_id: u32`** — `Obj::Enum` (`heap.rs:167`) holds two `Box<str>` per instance, both
     global. → id + names for Display only. Saves 2 allocs/inst.
  3. **Closure captures positional** — `Obj::Closure.captured: HashMap<String,Value>` (`heap.rs:178`)
     = HashMap alloc + string-hash per `GetCaptured` (`mod.rs:3354`). → `SmallVec<[Value;N]>` by
     per-proto slot (static per proto). Also speeds the per-spawn deep-clone.
  4. **GC mark-bit bitvec** — `Slot{obj,mark:bool}` (`heap.rs:234`) interleaves 1 B in 88 B; packed
     bitvec = dense sweep scan. Only if GC becomes hot (post-JIT).
  5. **Shrink `Obj` <88 B** — guard `chzstr.rs:205`; box rare big variants. **Trades against SSO** (sized
     to fill 88 B inline) → measure first.
  6. **HOF borrow-release clone** — `map`/`filter`/`fold` clone the list to release the heap borrow
     before `invoke_value`. A `Vm` split (`&mut ExecState` + `&Heap`) fixes it. Structural refactor.
  7. **`for`-loop snapshot (`ListClone`) + per-char alloc** — parity-blocked by interp snapshot semantics.
  8. **Operand-stack 16 B/Value traffic** → NaN-box (blocked) / register VM (low-ROI) — below.

- **🔵 Big / separate end-game tracks** (only once the language has truly stopped moving):
  - **Cranelift method-JIT** *(`future.md` §4 #6)* — the only path to *match/beat* CPython 3.14 on
    compute; counter-triggered, a whole backend. The stretch end-game. #1/#3/#2 above are its groundwork.
  - **NaN-boxing the `Value` (16 B → 8 B) — ⛔ BLOCKED by full `i64`** (`future.md:264`, `value.rs:18`):
    a full i64 + a type tag don't fit in 8 B (NaN payload ~48–51 bits). Was billed the biggest remaining
    lever; the i64 model rules it out without a tagged-small-int compromise.
  - **Register VM** — low ROI (dispatch already near the match floor).
  - **Generational / incremental GC** — low ROI (GC currently moves no bench).
  - **String concat/`split` builder** — medium lever; `join` already buffers (`mod.rs:4377`), `+`/`split`
    aren't benched yet.

- **⚪ `ref T` — transparent reference bindings** (DX sugar over `Ref[T]`, `r^ += 1` deref) — **PARKED
  behind the JIT** (`future.md:105`). Sugar, not capability; revisit post-JIT.

### 🟠 Deferred — will resolve later (real work, lower urgency)

Tracked in other docs; surfaced here so they aren't lost. None scheduled, but each is genuine backlog.

- **FFI surface expansion** (`std.cffi`, `src/native/cffi.rs`; spec.md §FFI, syntax.md:1232) — v1 is
  **scalars only**. Deferred: structs-by-value, callbacks / function pointers, varargs, opaque pointers /
  **userdata** (`Box<dyn Any>` for opaque `File`/`Regex` handles — io is whole-string today), `char*`
  ownership transfer / `free`. Needed for richer C interop / any future self-host.
- **Comprehension nested clauses** — `[x for x in xs for y in ys]` deferred (syntax.md:358); single-clause
  + guard shipped.
- **`std.cancel` tree propagation** — tokens are **flat** in v1; parent/child derivation (cancel a parent
  → cancel its children) is a documented follow-up (PROGRESS.md:244).
- **Graceful shutdown of accept loops** + a per-connection handler→acceptor signal channel — future work
  for long-running servers (concurrency-tier-d.md:297).
- **Reduction-constant tuning** (D3) — pick `CONTEXT_REDS` + per-op vs per-back-edge accounting
  (concurrency-tier-d.md:363).

### ⚫ May-or-may-not consider (revisit only if it bites; mostly by-design non-goals)

Recorded for completeness — likely stay as-is unless a real program forces the issue.

- **Concurrency, BEAM-flavored:** priority classes; restart/supervision policies (Elixir-style, out of
  scope C5). Narrow cross-nursery M:N limits beyond the coop-flatten above: contended shared channel
  (2+ receivers racing one channel — concurrent-divergent **by design**, never panics/hangs), inline-body
  *blocking* recv (case B — put it in a `spawn:`), eager-nursery cross-wake (concurrency.md §11).
- **`int32` / unsigned C ints** — no such scalar (feature-frozen by design; FFI widens at the boundary).
- **Defaults / named args on built-in methods** (`map`/`push`/`len`/…) — unsupported by design
  (syntax.md:216); user methods + free fns + ctors have them.

---

## 🟢 Verified working (so we don't re-flag)

- **Struct equality** (structural), **string indexing/ops** (`s[i]`, `len/upper/lower/trim/split/join/
  contains/starts_with`, `s.chars()`, `for c in s`), **list-of-structs + nested-list read**, by-ref list
  sharing across calls.
- **`if`/`match` as expressions** (incl. in interpolation), **`Result`/`Option` + `?`**, exhaustive-match,
  deep recursion, **integer overflow → recoverable panic** (never wraps), int-div truncation, `%` on negatives.
- **All std modules on both engines**, **recursive/self-referential structs** (BST, list) GC-clean,
  **mutable `self` across methods**, **nested-list DP**, **empty-map `K,V` inference from later use**.
- **User-struct iterator protocol** (`next() -> Option[T]`, lazy, early-`break`-safe) + **`Iterator[T]`
  parameterized bound** + **user parameterized protocols** (`protocol Container[T]`); lazy adapter structs
  compose without `yield`. `examples/iterator_bound.chz`, `iter_adapters.chz`.
- **Python-colon slicing** `xs[a:b:c]` — open bounds / step / reverse / negative index, on `list`/`str`,
  as assignment target. **Comprehensions** (list/set/dict + guard). **Tuple-destructuring `for`** +
  `enumerate`/`zip`. **Optional chaining `x?.f` + `??`**.

---

## Resolved log (one line each — full detail in `PROGRESS.md` + cited examples)

**Scripting essentials ✅** · comprehensions (`481514b`), Python-colon slicing (mid-M19, `src/slice.rs`,
`examples/slicing.chz`), `Iterator[T]` protocol (M13) + generators a permanent non-goal, `os.exit(code)`
(`481514b`, `examples/exit.chz`).

**Scripting ergonomics ✅** · list `.concat`/`.extend` + map `.merge`/`.update` (`concat_merge.chz`);
hex/bin/oct literals (`hex.chz`); tuple-`for` + `enumerate`/`zip` (`for_tuple.chz`); optional-chaining
`x?.f` + `??` (`optchain.chz`); `defer` block-scoped (M16→M18) + `defer:` block form (`defer.chz`).

**Type-system + runtime depth ✅** · non-const default exprs (relaxed; param-ref defaults still out);
calling a `fn`-typed field (`fn_field.chz`); `sort_by_key` (`sort_by_key.chz`); `Ref[T]` mutable box
(`std/ref.chz`, `ref.chz`); runtime stack traces (both engines identical, `stack_trace.chz`); integer
overflow policy (every `i64` overflow recoverable, `overflow.chz`); loop-var reassignment rejected;
`break`/`continue` in a `spawn:`/`defer:` block nested in an outer loop now rejected at check time
(`checker/mod.rs` save-zero-restore `loop_depth` across both block arms, ending a three-way `check`/VM/
interp divergence; 2026-06-16).

**Concurrency ✅** · pending-`spawn` drop on early `parallel:` escape → cancel-and-report (both engines);
VM nursery-leak on `?`/return escape (truncate to frame depth); **Path C** recv-in-native-callback
thread-demotion — #1 false-positive + #3 sleep/socket (`f828ef7`, D-tier complete; #2 same-box is the
WON'T-FIX carve-out above); **`std.cancel`** task cancellation/timeout `Token` + `Channel.trip()` latch
(flat tokens v1; `docs/concurrency.md` §6e); cross-nursery M:N flat scheduler (coop flatten still open above);
`Channel.close()`, per-socket `timeout_ms` (D6c), per-connection `spawn` all landed.

**Round 1 (#1–9) ✅** · index/field assignment, HOF params, list methods (`map`/`filter`/`fold`),
`map` type, literal/wildcard `match`, `break`/`continue`, tuples + destructuring, strict compound assign.

**Round 2 (#10–15) ✅** · `ord`/`chr`, `sort_by`, int `abs`/`min`/`max` (→ `std.cmp`), bitwise ops,
map iteration, nested/tuple match patterns.

**Milestones ✅** · M7 generics (fns/structs/protocols/enums, multi-bound) · M8 Tier-1 stdlib
(`json`/`time`/`fs`/`process`, `set`) · M9 Tier-2 (`regex`/`request`) · M10 type-depth
(`Hashable`/`Stringable`/operator protocols/type aliases) · M11 robustness (`recover:`, `T!` errors,
iterator protocol, match guards + ranges, default/named args) · M14 generics-depth (method type params,
user parameterized protocols, method defaults) · M15 slicing + `Index`/`IndexSet`/`Slice` protocols ·
**M20 in-language tests** (`assert`, `test fn`, `chezzi test` w/ suites/fixtures, `examples/assert.chz`).
**std.math fill** (`5a25a5c`: trig/exp/log + `pi`/`e`) · **std.request verbs** (put/patch/delete/head +
header map). **Tech debt:** parser `MAX_DEPTH` 128→64, dup type-param rejected, nested-`set` equality,
call-site type args, `?`-in-closure return checking.
