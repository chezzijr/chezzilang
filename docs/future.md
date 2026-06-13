# Chezzi — Future Directions (brainstorm, NOT scheduled)

> **Status:** speculative design notes. Forward-looking and opinionated. Nothing here is committed
> work — [`PROGRESS.md`](../PROGRESS.md) is the source of truth for what's actually scheduled and done.
> This doc captures *what would make Chezzi an effective scripting language* and *how to make it
> faster*, with verdicts and rough implementation shape. Most of §1–§3 has since **shipped** (noted
> inline); §4 (optimizations) is the live M19 backlog.

The language **core** is feature-complete (scalars, `list`/`map`/`set`/`tuple`, generic structs +
enums, `Result`/`Option` + `?`, generics + structural protocols, exhaustive `match`, closures/HOF,
modules, GC, two engines, interpolation, pipe, panic recovery via `recover:`, the `Iterator[T]`
protocol bound). What follows is the gap between "complete core" and "language you reach for to write
real scripts."

---

> **Promotion status:** §1 (`defer`) and the §3 scripting features have **shipped** (M15–M18); §2
> (concurrency) has **shipped through Tier-D**. They stay documented here for the design rationale.
> §4 (optimizations) is the live M19 backlog. See [`PROGRESS.md`](../PROGRESS.md) for landing detail.

## 1. `defer` (cleanup on scope exit) — ✅ **SHIPPED (M17)**, **block-scoped since M18** — see `examples/defer.chz`

> **M18 update:** shipped frame-scoped in M17, then moved to **block/lexical scope** — a `defer` runs
> when its enclosing indented block exits (loop body, branch, `recover:`, `match` arm, function body,
> module top level), not just at function return. Realises the "cleanup on scope exit" intent below
> more literally. See the M18 entry in `PROGRESS.md` and the `defer` section of `docs/syntax.md`.

Before M11 this was weak: no panic meant nothing to clean up after. **Now there is unwinding** —
the `recover:` boundary, `?` propagation, and runtime faults all unwind. So `defer` earns its keep
by running on **all three exit paths**: normal return, `?` short-circuit, and panic unwind. That is
exactly Go's value proposition.

**Implementation shape**
- Per-frame deferred-call stack, drained LIFO on *every* frame exit including unwind.
  - Interp: drain in the `Flow` / propagating channel path **and** the `recover` snapshot/restore path.
  - VM: drain at `Return` **and** inside the handler-stack unwind (`PushHandler`/`PopHandler` already exist).
- **Arg-evaluation timing:** evaluate `defer` arguments *at the `defer` statement* (Go semantics),
  not at exit. Less surprising; the deferred call closes over already-evaluated values.

**Alternative considered:** Python-style `with` (context-manager protocol `enter`/`exit`). More
Python-feel, but needs a new protocol + an indentation block. `defer` is simpler, adds no protocol,
and composes cleanly with `recover:`. **Recommend `defer`.**

---

## 2. Concurrency + parallelism — the shared-nothing (BEAM) model

> **Shipped through Tier-D.** The full design — `spawn`/`parallel:` nursery, `Channel[T]`,
> `Shared[T]`, sendability — lives in its own canonical doc **[`docs/concurrency.md`](concurrency.md)**,
> with phase history in [`docs/concurrency-tier-d.md`](concurrency-tier-d.md). Real OS-thread M:N
> engine via `--parallel`. **M-C implicit nurseries shipped** — bare `spawn` legal anywhere; every
> function body / module top level joins its tasks at `return`/end. Concurrency is feature-complete.

---

## 3. Missing features (ranked by leverage for scripting) → **mostly shipped (M12–M18)**

> Comprehensions, slicing, the iterator protocol, concat/merge, hex/bin/oct literals, optional
> chaining, tuple-destructuring `for`, match guards, `std.os.exit`, and runtime stack traces have all
> landed. Mutable closure capture was **resolved by decision** (kept snapshot-by-value by design, with
> `std.ref` `Ref[T]` as the idiomatic mutable box). Nothing in this list is still open — see
> [`PROGRESS.md`](../PROGRESS.md).

1. **Comprehensions** — `[x*2 for x in xs if x>0]` (+ dict/set). A Python-feel language without
   these feels broken. Pure parse-time desugar to loop + push. Cheap, large UX win.
2. **Sub-ranges — Rust-style `xs[1..3]`** — sub-list / substring via the existing `..` range
   (half-open), no new lexer token, no step. `Slice { obj, start, end }` → container-typed →
   bounds-clamped range-copy. (Omitted bounds / `..=` / negative index are deferred extensions.)
3. ~~**Iterator protocol + generators (`yield`)**~~ — **iterator DONE; generators removed.** The
   `Iterator[T]` parameterized protocol shipped (M13): user structs usable in `for`, generic
   `[S: Iterator[T], T]` bounds, and lazy `map`/`filter`/`take` written as **adapter structs** over it
   (Rust `std::iter` model — `examples/iter_adapters.chz`). **`yield`/generators are a permanent
   non-goal** (see `spec.md` *Non-goals*): they would need coroutine/continuation support in both
   engines, and the adapter-struct pattern covers lazy streaming without it.
4. ~~**List concat + map merge**~~ — **DONE.** Method-based: list `.concat`/`.extend`, map
   `.merge`/`.update` (concat/merge new, extend/update mutate). No new syntax; spread/unpack stays
   dropped. `examples/concat_merge.chz`.
5. ~~**Hex / binary / octal literals**~~ — **DONE.** `0xFF`/`0b1010`/`0o17`, lexer-only via
   `i64::from_str_radix`, `_` between digits. `examples/hex.chz`.
6. ~~**Optional chaining + null-coalescing**~~ — **DONE.** `x?.field`/`x?.method()` + right-assoc
   `a ?? b` on `Option`, lowered to a `match` by the desugar pass (zero checker/engine code).
   `examples/optchain.chz`.
7. ~~**Tuple-destructuring `for` (+ `enumerate` / `zip`)**~~ — **DONE.** `for a, b in list[(A,B)]`
   (N-var over `list[tupleN]`); VM splits map vs list-of-tuples at runtime on a new `Op::IsMap`.
   `enumerate`/`zip` shipped as pure-Chezzi `std/iter.chz`. `examples/for_tuple.chz`.
8. ~~**Mutable closure capture**~~ — **RESOLVED by decision.** Capture stays **snapshot-by-value**
   *intentionally* (a bare scalar is copied when a closure closes over it); the idiomatic mutable box
   is `std.ref` `Ref[T]` (a one-field struct, shared by reference, mutated through a method). Documented
   loudly in `std/ref.chz`. So closure counters / accumulators *are* expressible — via `Ref[T]`, not a
   raw captured `int`.
9. **Match guards + range patterns** — `n if n>0:`, `1..10:`. Roadmap. Guards subsume the rest.
10. ~~**`std.os.exit(code)` + real exit codes**~~ — **DONE.** `std.os.exit(code)` is a hard, uncatchable
    halt (unwinds past `recover:`, bypasses `defer`), with the code threaded through both run drivers +
    the CLI; exit-wins precedence holds under `--parallel`. `examples/exit.chz`.
11. ~~**Runtime stack traces**~~ — **DONE.** Error + call chain + line numbers, both engines
    (`37f374a`).

**Ecosystem (Tier 4, separate track):** REPL (huge for scripting iteration), formatter, `assert` +
built-in test runner, LSP.

---

## 4. Optimizations (ranked effort → payoff)

> **Live numbers:** `docs/benchmarks.md` tracks Chezzi vs CPython (reproducible via
> `benches/run.chz`). After the M19 phases (call-flatten + SSO incl.): **~1.3×–3.5× slower than
> CPython**, and a **standing startup win** (~11× faster cold). The gap scales with call density —
> `loop` (no calls) is 1.32×, `fib` (all calls) is 3.54×. The M19 levers below are marked landed;
> the **ranked not-started backlog is "Post-M19 next levers"** further down.

Current: ~4–6.5× over the tree-walker, near the safe-match-dispatch floor. The two real costs are
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
  deeper structs), not when method dispatch dominates. **Open follow-up:** a struct **type-id guard**
  (stamp a numeric type id on `Obj::Struct`; guard on `obj.tid == cell.tid` — a pure-int compare with
  no name re-verify) would tighten the hot path and could close the shallow-struct caveat; it costs a
  `tid` field + the struct construction / snapshot / wire sites, so it was deferred out of P4.
- ✅ **Kill per-call clones in `invoke_value`** — *landed M19 Phase 1*: matches on `&Obj` (no whole-
  `Obj` / closure-`HashMap` clone) and drops the arity-check `name.clone()`. Cut `fib` −17%, `list`
  −22%.
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
  whole `run_until` instead of per call. **Hard part / parity risk:** today pause/park (B1/D3),
  `recover:` unwind, and `defer` all lean on Rust-stack unwinding through the nested `do_call`/`?`
  chain. A flat loop must instead park by leaving `self.frames` intact and breaking the loop (the M:N
  engine already saves/restores frame state via `FiberCtx`, so the machinery exists). **Keep the
  re-entrant `run_proto_in_place` for native-initiated calls** (HOFs — `map`/`filter`/`sort` call
  `invoke_value` per element and need the callback's result *synchronously* mid-native-method); only
  the bytecode `Op::Call` path flattens. The two coexist: HOFs nest a sub-loop when they must, the
  common recursive/bytecode call no longer does. Cheap warm-up that stands alone even without
  flattening: **hoist the per-call `Arc::clone`** (raw-pointer/restructure the program borrow) — a free
  few-percent on every call-bound bench. Blast radius is **VM-only** (the frozen interp is untouched);
  parity is testable against the existing fib / recover-in-recursion / defer-in-recursion / deep-
  recursion-overflow goldens. Bigger than a Medium item, smaller than the register-VM rewrite below.
- **NaN-boxing the `Value` — BLOCKED by full 64-bit ints (2026-06-12 reality-check).** The goal
  (16 B → 8 B, operand-stack cache density, moves `loop`/`list`/`fib`) is real, but `Value::Int` is a
  **full `i64`** (`src/vm/value.rs:18`). NaN-boxing packs every value into 8 bytes, and a full i64 +
  a type tag do **not** fit in 8 bytes alongside `f64` — the payload of a NaN-box is ~48–51 bits. To
  do it you must **box big ints** (small-int tagging): a branch + a heap alloc on every int outside
  the taggable range, plus a semantics-sensitive overflow path — i.e. *not* behavior-preserving for
  free, and an uncertain net win on the very int-heavy benches it targets. **Lua 5.4 made exactly
  this call** — it stayed at a 16-byte tagged union *because* it added 64-bit ints. Blast radius is
  **VM-only** (the frozen interp has its own `Rc`-based `Value` in `src/interp/value.rs`, untouched),
  but it's still a milestone-sized design spike (box-big-ints scheme + measure), not a clean
  behavior-preserving session. Park until the int model is up for revisiting.
- **Register VM** instead of stack — fewer ops, less stack traffic. Effectively a VM rewrite; only
  if dispatch count is still the wall after superinstructions.
- **Generational / incremental GC** — current is stop-the-world full-heap (`next_gc = 2×live`).
  Generational cuts pause + rescan cost on allocation-heavy scripts.
- **Cranelift AOT/JIT** — already the stretch goal. Near-native, but a whole backend. Only after the
  language stops moving.

**Highest payoff-per-effort (original M19 batch, all landed):** superinstructions + inline caching +
peephole/const-fold. They attacked dispatch count and name lookup — the two actual costs — without
touching the value model or the GC.

### Post-M19 next levers (ranked, not started — diagnosed 2026-06-12)

The M19 cheap batch + call-flatten + SSO are spent. Latest gap tracks **call density**: `loop` (no
calls) **1.32×**, `primes` 2.53×, `str` 2.65×, `struct` 2.71×, `map` 2.83×, `list` 2.97×, `fib` (all
calls) **3.54×**; startup **0.094× (11× win)**. The bottleneck is **call overhead + per-op dispatch +
a few alloc paths — NOT the value model or the GC** (confirmed: `loop` is already at the match-dispatch
floor; ints are unboxed; GC is share-nothing per-thread and moves no bench). Target is **CPython 3.14**
(specializing adaptive interpreter + optional copy-and-patch JIT), so the interpreter can *narrow* the
gap but a JIT is the only path to *match/beat* it on tight compute.

**Tier 1 — interpreter, cheap→medium, behavior-preserving, each hits a measured bench:**

1. **Method-call IC + flatten `do_method_call`** *(hits `struct` 2.71× + all OO)*. `do_method_call`
   (`mod.rs:~3868`) still string-looks-up `def.methods.get(method)` per call **and** still recurses into
   a fresh `run_until` — call-flatten only covered `do_call`'s plain-fn fast path (see its own follow-up
   note). Add a per-call-site monomorphic cache (`tid → proto`, the same shape as the landed `field_ic`)
   and push the method frame in place. Symmetric to the field IC; reuses that machinery.
2. **Trim per-op overhead in `run_until`** *(hits the dispatch floor: `loop`/`primes`)*. Three things run
   **every instruction** that are pure overhead on the serial (default, benchmarked) engine:
   - `span = proto_ref.lines[ip]` (`mod.rs:2157`) is loaded every op but used **only on fault** → pass
     `(pid, ip)` to the error path and reconstruct the span lazily there.
   - the `if self.mn.is_some()` reduction-count branch (`mod.rs:2137`) + the cancel check (`mod.rs:2122`)
     are MN-only → split a lean serial loop body from the MN body (or hoist them off the serial back-edge).
   - `self.step(op, span)` is a **separate fn call per opcode** → inline the ~6 hottest ops (GetLocal, the
     superinstrs, Jump, Call, Return) directly in the loop, delegate the long tail to `step`.
3. **Call-site specialization for `Op::Call`** *(hits `fib` 3.54×)*. Each call re-checks Func/Closure/
   Native, derefs the heap callee, and re-validates arity. Cache the resolved proto at the call site keyed
   by callee identity (CPython `CALL_PY_EXACT_ARGS`); skip the dispatch + deref + arity recheck on the
   monomorphic common case. fib's remaining tax after call-flatten.

**Tier 2 — structural, medium→large:**

4. **Adaptive opcode quickening (PEP 659)** *(the single most CPython-like lever)*. The generalization of
   Tier-1 + the existing static superinstructions/ICs: after an op runs once, rewrite it in place to a
   type-specialized form (`Add`→`AddIntInt`, `GetIndex`→`GetIndexList`, `CallMethod`→cached) behind a
   deopt guard. Unifies superinstructions + IC + call specialization under one mechanism. **Constraint
   (same one P2b/P4 hit):** bytecode is shared `Arc<Program>` read-only across `--parallel` workers, so
   quickened cells must live in a per-`Vm` side table keyed by site, not mutate the `Op`.
5. **map/list index specialization** *(hits `map` 2.83×, `list` 2.97×)*. `GetIndex`/`SetIndex`
   (`mod.rs:~7649`) are dynamic type-checked dispatches with no caching; specialize per shape — falls out
   of #4.

**Tier 3 — big, separate milestones:**

6. **Cranelift method-JIT** — the only path to *match/beat* CPython 3.14 on compute. Counter-triggered,
   JIT the hot protos (Python's tier-2 model). End-game; only once the language is fully frozen. #4 is the
   lower-risk stepping stone toward it.
7. **NaN-boxing — stays BLOCKED** (full i64; see the dedicated note above).
8. **Register VM / generational+incremental GC — low ROI** (dispatch is already near the match floor; GC
   moves no bench). Deprioritized; revisit only if a real workload proves otherwise.

**Sequencing:** do **Tier 1 in order (1→2→3)** as the next perf batch — all cheap-to-medium,
behavior-preserving, two-engine-parity-clean, each targeting a named bench (expected: fib ~3.5×→~2.2×,
struct ~2.7×→~1.9×, loop/primes shaved toward the floor). Then choose **#4 (adaptive quickening)** as the
high-ceiling interpreter play or **#6 (Cranelift)** as the JIT end-game.

**M19 Phase 1 done (2026-06-11):** peephole/const-fold + superinstructions + `invoke_value` clone
kill — all behavior-preserving (1516 tests + full two-engine parity green). Results in
`docs/benchmarks.md`.

**M19 Phase 2 done (2026-06-11):** in-place call args in `do_call` (per-call `Vec` gone, `fib` −13%)
+ `stringify`-into-buffer for `BuildStr` (`str` −5%) — both behavior-preserving (1518 tests + full
two-engine parity green, 4-agent S++ panel clean). Results in `docs/benchmarks.md`. Remaining `str`
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
`ModuleInline`/`ModuleAlias` replay must round-trip slots, not names. This needs its own two-engine
parity pass + the `--parallel` module-fault tests, so it is scheduled separately rather than bundled
with the Phase 2 allocation kills.
