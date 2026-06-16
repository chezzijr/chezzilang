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
> **Forward-looking brainstorm** (BEAM concurrency, JIT, register VM, NaN-boxing) lives in
> [`docs/future.md`](docs/future.md) — speculative, NOT scheduled.

---

## Open gaps — START HERE

### 🟡 Type-system + runtime depth

- **🟡 `break`/`continue` inside a `spawn:` / `defer:` block — three-way divergence (the one real bug).**
  These blocks compile to a fresh child proto with an empty loop stack, so a `break`/`continue`
  lexically nested in an enclosing loop diverges: `check` → `ok`, VM → runtime `break/continue outside
  loop`, `--interp` → silently treats it as a block exit. A clean `check` must guarantee the program
  runs and both engines must agree. Affects both block forms. **Confirmed HEAD 2026-06-16**
  (`/tmp/brk_defer.chz`, `/tmp/brk_spawn.chz`). **Fix:** make the block a control-flow boundary in the
  checker — save/zero `loop_depth` across `check_block` for these arms (`checker/mod.rs:1092`) so the
  `loop_depth == 0` guard at `StmtKind::Break` fires, rejecting at check time on both engines. One
  shared fix.

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

- **🟡 Cross-nursery wakeups — cooperative-engine flatten pending.** RESOLVED under `--parallel` (M:N
  flat scheduler — multi-level nesting + late-spawn, `examples/parallel_cross_nursery_circular.chz`);
  the **cooperative** engine still can't flatten a fiber in an outer nursery woken by an inner one
  (`docs/concurrency-tier-d.md:342`). Output correct under `--parallel`; the gap is coop-only liveness.
- **🟡 Cooperative + `--interp` engines cannot preempt CPU-bound tasks.** They switch fibers only at
  yield points (channel ops, blocking `recv`, back-edge-to-scheduler). A pure-CPU loop with no channel
  op monopolizes the single thread → a sibling canceller/timeout never runs. **Same source diverges by
  engine:** a 2e9-iter CPU worker under manual-cancel aborts mid-flight on `--parallel` (~0.5s) but runs
  to completion on coop/`--interp` (~69s). This is why cancellation examples carry no golden `.expected`
  (`examples/parallel_cancel.chz`). **Fix:** reduction-counting preemption exists on M:N (D3, `vm:2871`);
  coop would need a back-edge yield budget — weighed against the frozen oracle's determinism.
- **🟡 `Shared.update` same-box hold-and-wait — WON'T FIX by design.** A `recv` blocking inside
  `update(f)` holds `update_lock` while parked → a sender needing the *same* box deadlocks. The universal
  hold-and-wait class (Go #13759 global-only; Rust `clippy::await_holding_lock`; BEAM no shared locks).
  Rule: don't block on a value needing the same `Shared` box. Future: a lint when the tooling track lands.
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
overflow policy (every `i64` overflow recoverable, `overflow.chz`); loop-var reassignment rejected.

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
