# Chezzi — Progress Tracker

Single source of truth for "what am I doing next." Update after every work session.

**Legend:** ⬜ not started · 🟦 in progress · ✅ done

> **Mode:** Claude implements directly — working, tested code each session (see `CLAUDE.md`).
> Full per-milestone detail lives in git history; this file is a forward-looking tracker, not a changelog.

---

## Current focus

**✅ M19 — Small-string optimization (SSO) LANDED (2026-06-12).** `Obj::Str` now holds a
`ChzStr` (`src/vm/chzstr.rs`) instead of a `Box<str>`: strings ≤ `INLINE_CAP` (22 UTF-8 bytes)
live **inline** in the variant — no per-value heap `Box` alloc — and longer strings spill to a
`Box<str>`. The `str` bench's 500k `"item-N"` parts are all ≤11 bytes, so building them no longer
touches the allocator per element. `ChzStr` impls `Deref<str>` + `From<&str>/<String>/<Box<str>>`,
so the ~100 `Obj::Str` match arms and `"x".into()` test constructors compiled **unchanged**; only
~8 real construction sites switched `.into_boxed_str()` → `.into()`. `Clone`/`Eq`/`Hash` delegate
to `as_str()` so map keys / interning / `==` stay byte-identical. `size_of::<Obj>()` is **unchanged
at 88 B** (pinned by a guard test — `Module`/`Closure` still dominate). **Result: `str`
217→174 ms, 2.62×→2.10× CPython (−20%); `list`/`loop`/`fib` neutral (no regression).** TDD'd:
`ChzStr` selection unit tests (inline/heap boundary, multi-byte UTF-8, Eq/Hash content semantics)
proven red-then-green, a `vm_alloc_str_inlines_short_spills_long` wiring guard, and a
`sso_boundary_string_ops_parity` two-engine test (concat/split/join/index/iterate/`==`/map-key
straddling the 22-byte boundary). **1565 tests** green, conformance 7/7, `clippy --all-targets`
clean. VM-only; frozen interp untouched. Closes the **small-string optimization** lever in
[`docs/future.md §4`](docs/future.md). The pure-int `list` bench stays at the **parity floor** —
`for x in xs` snapshots the list (`Op::ListClone`) and the frozen interp snapshots identically
(`exec_for` → `iter_rows_from_value`), so the clone can't be dropped without diverging; ints are
already unboxed `Value`, so there is no per-element box to kill there.

**✅ M19 — `Arc::clone` warm-up hoisted out of `run_until` LANDED (2026-06-12).** The call-flatten
(`634c6f5`) had already killed the *per-call* `run_until` recursion, leaving one
`Arc::clone(&self.program)` at the loop ENTRY (`mod.rs:2095`) — a per-entry atomic that was pure
borrow-checker tax. Replaced with a raw `*const Program` borrow (sound: `self.program` is an
immutable `Arc` never reassigned after `Vm::new` — verified; `swap_ctx` swaps heap/frames/stack, not
`program`). Post-flatten this entry is hit per top-level / native-reentry (HOF callbacks,
operator-overload `compare`, deferred calls) / fiber-resume — **not** per call — so it's **neutral on
the no-HOF standard suite** (as predicted) but **1.05× on callback-heavy code**. Verified with a new
`benches/chz/hof.chz` (A/B 30 runs, 383→363 ms) and guarded by a `native_reentry_hof_compare_defer_parity`
unit test (HOF + operator overload + defer-in-recursion, VM == interp). VM-only; frozen interp
untouched. **1552 tests** green, conformance 7/7, `clippy --all-targets` clean. This closes the
"cheap warm-up" item under the call-flatten lever in [`docs/future.md §4`](docs/future.md).

**Concurrency is feature-complete (confirmed 2026-06-12).** Roadmap landed through **Tier-D** (D0–D6);
the per-socket read/accept/write **timeout** that `concurrency-tier-d.md` listed as "still deferred"
is in fact **already shipped (D6c)** — `examples/socket_timeout.chz` + poller deadline tests; that
doc line was stale and is now corrected. The only remaining concurrency item is **M-C implicit
nurseries**, deliberately *deferred* (an invisible "joins at end of function" barrier hides *when*
work runs while execution is observable-sequential). No new concurrency capability is open.

**🟦 M19 Perf track — Phase 1 LANDED (2026-06-11).** Three behavior-preserving optimizations, all
TDD'd and guarded by the full two-engine parity suite:
1. **Killed the per-call clone in `invoke_value`** — was `self.heap.get(h).clone()` on the whole `Obj`
   (a closure's captured `HashMap` included) plus an arity-check `name.clone()` every call. Now matches
   on `&Obj`, copies out the `Copy` fields, defers the name to the error path. → `fib` −17%, `list` −22%.
2. **Jump-relocating peephole pass + constant folding** (`src/compiler/peephole.rs`) — one forward
   walk folds `ConstInt`/`ConstFloat` arith + `Neg`/`Not` (replicating the VM's checked semantics:
   overflow / div-by-zero stay unfolded → identical runtime error), tracking an old→new index map and
   renumbering every absolute jump. A window is refused if its interior is a jump target.
3. **Superinstructions** (`Op::BinLocalLocal` / `BinLocalConst` / `IncLocal`, `src/vm/op.rs`) — fuse the
   hot `GetLocal+GetLocal+BinOp`, `GetLocal+Const+BinOp`, and `i += k` windows. Int fast path inlined;
   any other operand type falls back to the exact unfused `arith`/`compare_op` (struct overloading /
   string concat / float promotion / fiber parking stay byte-identical). Bodies live in `#[inline(never)]`
   helpers so `step`'s frame stays lean (it's on the deep-recursion cycle). → `loop` −36%, `primes` −25%.

Result: gap to CPython narrowed **loop 2.1×→1.4×, primes 3.5×→2.6×, list 3.8×→2.9×, fib 5.9×→4.9×**
(startup still ~11× win). Full numbers in [`docs/benchmarks.md`](docs/benchmarks.md); backlog status in
[`docs/future.md §4`](docs/future.md).

**🟦 M19 Perf track — Phase 2 LANDED (2026-06-11).** Two behavior-preserving allocation kills, TDD'd
and guarded by the full two-engine parity suite + a 4-agent S++ review panel (all clean):
1. **In-place call args** — `do_call`'s `Func`/`Closure` fast path runs directly over the args already
   on the operand stack (a `copy_within` drops the callee from beneath them), killing the per-call
   `split_off` `Vec` alloc + the re-push in `push_frame`. `push_frame` was refactored into shared
   `frame_depth_guard` + `finish_frame` helpers so the `Vec` and in-place paths raise the identical
   overflow error and install byte-identical frames. Native / non-callable callees keep the `Vec` path
   (`invoke_native` needs an owned `Vec`; HOFs build args off-stack). → `fib` −13%.
2. **`stringify`-into-buffer** — `stringify`/`stringify_obj`/`stringify_seq` rewritten to append into a
   caller-owned `String` (`stringify_into` &co); `BuildStr` reuses one buffer across all interpolation
   parts instead of allocating an intermediate `String` per part. Byte-exact output. → `str` −5%.

Result: gap to CPython narrowed further **fib 4.9×→4.2×, str 3.5×→3.15×, primes 2.6×→2.54×,
loop 1.4×→1.30×** (list flat, not call-bound; startup still ~11× win). **1518 tests** green + `cargo
test conformance` (7/7) + `cargo clippy -- -D warnings` clean. Numbers in
[`docs/benchmarks.md`](docs/benchmarks.md).

**🟦 M19 Perf track — Phase 2b LANDED (2026-06-11).** **Global-slotting** — the inline-cache
equivalent for name lookup, TDD'd and guarded by the full two-engine parity suite (incl. the
`--parallel` module-fault tests). The compiler now assigns every module global a stable `u32` slot
(`ModuleProto.global_slots`, collected before any code is emitted so forward refs resolve) and emits
`GetGlobalSlot`/`SetGlobalSlot`/`DefineGlobalSlot`; the old name-keyed ops are gone. `Obj::Module`
became `{ slots: Vec<Value>, index: HashMap<Box<str>,u32> }` — slot ops hit `slots[i]` with no hash;
the `index` still backs `module.member` reads, imports, native-module population, and errors. The
run driver pre-sizes a module's slots from `global_slots`; native modules + worker fault-replay grow
slots by name via `module_define`. Because the slot map lives in the shared `Arc<Program>`,
parent and faulted-worker agree on slot↔name **by construction** — this *removes* the latent
HashMap-iteration-order fragility the snapshot path would otherwise have inherited (snapshot now
emits globals in slot order via `module_slot_pairs`).

Result: **`fib` 4.2×→3.78× (−9%)** — the call-heavy bench, where `fib`'s per-call callee resolution
went from a name-keyed map probe to a `Vec` index. The other microbenches are flat within noise:
their hot loops read *locals*, not globals, so global-slotting has nothing to bite on there (the
a-priori "moves `primes`" guess was wrong — `primes`' hot path is inner-loop integer arithmetic, one
global call per *outer* iteration). No regressions (the slot read is strictly cheaper than the probe
it replaced). **1520 tests** green + `cargo test conformance` (7/7) + `cargo clippy -- -D warnings`
clean. Numbers in [`docs/benchmarks.md`](docs/benchmarks.md).

**🟦 M19 Perf track — Phase 3 LANDED (2026-06-11).** Two behavior-preserving allocation kills on the
string path, TDD'd and guarded by the full two-engine parity suite + a 2-agent review panel (both
verdicts SOUND / behavior-preserving):
1. **`ConstStr` interning** — `Op::ConstStr` pushed a freshly `clone`d, boxed, heap-allocated `Obj::Str`
   *every* time. Now a per-heap cache keyed by the literal's **data pointer** (`s.as_ptr()`, stable for
   the program's lifetime since the `String` lives in the immutable `Arc<Program>`) reuses the
   already-allocated handle — first push allocs + caches, every later push of the same op is a pointer
   lookup. Sound because `Obj::Str` is never mutated in place and there is no identity operator, so
   aliasing is unobservable. The cached `GcRef`s are GC roots (`Vm::collect`) so they're never swept; the
   cache is heap-keyed, so an M:N fiber swaps it WITH its heap in `swap_ctx` / carries it in `into_fiber`
   — exactly mirroring `module_objs`/`executors`. → `str` −17% (the str bench re-pushes literal chunks
   ~500k times).
2. **Per-char single-alloc** — one shared `alloc_char(c)` helper (`Box::<str>::from(c.encode_utf8(&buf))`,
   one alloc) replaces the `c.to_string().into_boxed_str()` two-alloc pattern at every 1-char-string site:
   string for-iteration (`ListClone` Str arm, now `Vec<char>` not `Vec<String>`), `chars()`, string
   indexing `s[i]`, and `chr(n)`. Byte-identical output; speeds string-iteration workloads (not in the
   bench set directly).

Result: **`str` 3.24×→2.71× (227 ms, was 273 ms)**; other benches flat within noise (interning only bites
where the same literal op repeats — fib/loop/primes/list don't). No regressions. **1525 tests** green +
`cargo test conformance` (7/7) + `cargo clippy -- -D warnings` clean. Numbers in
[`docs/benchmarks.md`](docs/benchmarks.md).

**🟦 M19 Perf track — Phase 4 LANDED (2026-06-12).** **Struct-field inline cache** — the other half of
name-lookup ICs (P2b did globals). `GetField`/`SetField` now carry a per-call-site IC id into a per-`Vm`
`field_ic` vector that caches the field's index; a hit re-verifies the cached index against the live
field name (`fields[idx].0 == name`) and collapses the `O(field-position)` name-probe to one verify-
compare; a miss re-probes and refills. Static slotting (P2b's model) is impossible — **the compiler is
type-erased**, so the field's struct type is unknown at emit time — hence a *runtime* IC. Sound +
thread-safe by construction: the cell holds an index, **not a `GcRef`**, so it's invisible to GC /
snapshots / `swap_ctx` (none of that machinery changed); cooperative fibers run sequentially and
`--parallel` workers each own a `Vm`; every access self-verifies, so a stale/cross-type cell can never
return a wrong field. The frozen interp is tree-walk (never sees the opcode) ⇒ parity automatic. Tuple/
numeric element access (`t.0`) gets a `NO_IC` sentinel and stays zero-overhead.

Result: new **`struct`** bench (field-access-bound, 8-field accumulator) **3.32×→2.89× (477 ms, was
549 ms, −13%)**; non-struct benches unchanged (`Op` size unchanged, IC never engages). **Measured
honestly:** a *method-bound* shallow-field bench (6-field particle, hot op is a `self.*` call) showed
the IC **~neutral to −3%** — the cold `field_ic` indirection isn't amortized when field access isn't the
bottleneck. The IC wins where field resolution actually dominates (wider/deeper structs); a struct
**type-id guard** (pure-int compare, no name re-verify) is the logged follow-up. **1541 tests** green +
conformance (7/7) + `clippy -- -D warnings` clean. Numbers in [`docs/benchmarks.md`](docs/benchmarks.md).

**🟦 M19 Perf track — Phase 5a LANDED (2026-06-12).** **FxHash map/set index hasher.** `MapData`/
`SetData`'s `index` (`cached-hash → positions`) and `str_intern` (pointer-keyed) swapped stdlib SipHash
for a tiny in-tree FxHash (`src/vm/fxhash.rs`, no new dependency). The hasher only routes the probe;
`values_equal` confirms every hit ⇒ behavior-preserving, VM + interp parity (new tests lock map int/str
keys, a constant-`hash()` collision struct, set ops). Maps/sets were **unbenched** — added a `map` bench
(200k int inserts + 1M lookups). Result: **`map` 3.04×→2.82× (234 ms, was 252 ms, −7%)**; other benches
flat (none touch map/set). **Footgun caught by measuring:** a naive multiply-only FxHash was **100×
slower** — int keys store `f64::to_bits` (low mantissa bits zero), and FxHash mixes entropy only upward,
collapsing hashbrown's low-bit bucket index → O(n) probes; a splitmix64 finalizer in `finish()` fixed it.
**1547 tests** green + conformance (7/7) + `clippy -- -D warnings` clean.

**🟦 M19 Perf track — Phase 5b LANDED (2026-06-12, measured NEUTRAL).** **Struct type-id guard** — the
logged P4 follow-up. Every `Obj::Struct` now carries a dense `tid` (layout id from `StructDef::tid`,
assigned in declaration order at compile); the field IC hit guards on `cell.tid == obj.tid` (pure-int
compare) instead of P4's `fields[idx].0 == name` string re-verify. Sentinel `TID_NONE` (unregistered/
native structs, empty cells) never matches → can't false-hit across distinct unregistered layouts.
VM-only ⇒ parity automatic. **Result: neutral** (struct 1.02×, method-bound 1.01× — both in noise),
**no regression**. P4 had already collapsed the name-probe to a *single* verify-compare, and for short
field names that string compare is already cheap, so swapping it for an int compare saves nothing
measurable — the predicted "shallow-struct caveat" was a guess, not a measured cost. **Kept** as the
principled guard (removes the last string compare from the field hot path, future-proofs polymorphic
sites); the field-IC lever is now **spent**. **1549 tests** green + conformance (7/7) + clippy clean.

**🟦 M19 Perf track — call-flattening LANDED (2026-06-12).** The top call-bound lever, TDD'd
(red→green) under the two-engine parity suite. Every Chezzi call recursed into a **fresh Rust
`run_until` loop** (`do_call` → `run_proto_in_place` → `run_until`), costing a native Rust stack frame
**and** an `Arc::clone(&self.program)` per call. The bytecode `Op::Call` fast path now **pushes the
callee frame and lets the running `run_until` loop execute it** (CPython-3.11 "zero-cost frames");
`do_return` already pushes the result to the caller stack + pops the frame, so the loop continues with
no synchronous result to thread back. The dispatch loop is **unchanged** — it advances `ip` on the
captured caller frame *before* `step`, so the pushed frame (at `frames.len()-1`, `ip=0`) runs next;
pause/`recover:`/`defer` are caught by the loop body's own checks (they operate on `self.frames`, not
the Rust stack). HOFs / struct methods keep the re-entrant `run_proto` (they need the callee result
synchronously mid-Rust-method); the flat bytecode loop and the nested sub-loops coexist. Dead
`run_proto_in_place` removed. **Result: `fib` 3.85×→3.54× (−8%, the worst/most call-bound bench),
`list` 3.16×→2.97× (−6%); `loop`/`primes`/`str` flat** (no-call / arith-bound / alloc-bound — exactly
the predicted shape). Modest because flattening removes only the per-call recursion + atomic, not the
per-op dispatch of the call body. **Robustness bonus:** deep *plain* recursion no longer consumes host
stack — bounded by `MAX_CALL_DEPTH` (10_000), not the 256 MiB `VM_STACK_BYTES` thread; a recursion that
SIGABRT'd a 1 MiB stack pre-change now completes (guarded by `deep_plain_recursion_runs_on_small_host_stack`).
**1550 tests** green + conformance (7/7) + `cargo clippy -- -D warnings` clean. Numbers in
[`docs/benchmarks.md`](docs/benchmarks.md). **Follow-up:** flatten `do_method_call` (still `run_proto`)
for the `struct`/method benches.

**✅ `defer:` block form LANDED (2026-06-11).** Ergonomic gap closure (was 🟡 in `gaps.md`), TDD'd and
two-engine-parity-clean. `defer` now takes an indented block as well as a single call — multi-action
cleanup without N `defer` lines:
```chezzi
defer:
    log("closing")
    conn.close()
```
Mirrored `spawn`'s dual form 1:1 with **no new VM op**: AST `Defer(Expr)` → `Defer(DeferTarget::{Call,
Block})`; `parse_defer` branches on `:` → `parse_block`; grammar `<deferStmt>` gained `| "DEFER"
<block>` and moved into `<compoundStmt>` (conformance 7/7 green); checker splits the arm (Block =
ordinary nested scope, **no** capture floor — same-thread, so captures are not read-only, unlike a
`spawn:` block); compiler's `compile_defer` Block arm emits **`MakeClosure(pid, entries)` +
`DeferCall(0)`** (reuses existing ops); interp added `Deferred::Block` snapshotting locals **shallow
(`.clone()`, matching `MakeClosure`'s handle copy — NOT the spawn airlock's `deep_clone`)** so both
engines agree on container aliasing. **Semantics:** body runs top-to-bottom at scope exit; LIFO **as a
unit** relative to other `defer`s; free vars snapshot **by value at the `defer` point**; runs on all
exit paths (return/`?`/break/continue/panic/`recover:`). A 2-reviewer panel caught **two parity bugs**,
both fixed before landing: (1) reassigning an enclosing local inside the block crashed the VM compiler
(no `SetCaptured` op) and silently no-op'd the interp → now rejected at check time via a dedicated
`defer_floors` write-gate (separate from `capture_floors`, so same-task non-sendable *reads* stay
legal); (2) a `?` short-circuit inside the block leaked a runtime error on the interp but was discarded
on the VM → interp's `run_block_task` now absorbs the propagation like `call_closure`, so both engines
discard it. Tests: `examples/defer.chz` golden (VM == interp == `.expected`) extended with block-form +
snapshot + `?`-path cases; 4 VM parity tests + 5 checker tests + 2 parser tests. **1535 tests** green +
conformance (7/7) + `clippy --all-targets` clean.

**Next (M19) — backlog reality-check (2026-06-12).** A pass over the three big-ticket levers before
picking the next task (full note in [`docs/benchmarks.md`](docs/benchmarks.md) + [`docs/future.md §4`](docs/future.md)):
- **NaN-boxing `Value` is BLOCKED by full 64-bit ints, not "next."** `Value::Int` is a full `i64`
  (`src/vm/value.rs:18`); an i64 + a type tag don't fit in 8 bytes alongside `f64`, so it needs
  **boxed big ints** (branch + alloc per int, semantics-sensitive overflow) — *not* behavior-preserving,
  uncertain win on the very int benches it targets. **Lua 5.4 stayed 16-byte for this exact reason.**
  Blast radius is **VM-only** (the frozen interp has its own `Rc`-based `Value` in `src/interp/value.rs`
  — the earlier "touches every match across VM + interp" was wrong), but it's still a milestone spike.
  Parked.
- **String concat/split builder/rope moves no bench.** The `str` bench is `BuildStr` + `,".join`, and
  `join` already buffers into one `String` (`mod.rs:4377`); `+`/`split` aren't exercised. The real open
  `str` lever is **small-string optimization** (inline ≤N-byte strings in the `Obj` slot — `alloc_str`,
  `mod.rs:4697` — killing the per-element `Box<str>` alloc).
- **Arith specialization + frame pooling: effectively closed** — P1 superinstructions inline the
  monomorphic int path; `CallFrame`'s `Vec`s are alloc-free (no per-call frame alloc to pool).

**Real next levers** (contained, parity-safe, bench-moving): a struct **type-id guard** for the field IC
(pure-int compare, no name re-verify — closes the P4 shallow-struct caveat), **small-string optimization**,
and a faster usize hasher. Ranked backlog in [`docs/future.md §4`](docs/future.md).

**Next (M19) — top remaining lever DIAGNOSED (2026-06-12): flatten the call loop.** With the cheap
dispatch/name-lookup batch spent (type-id guard P5b ✅ neutral, FxHash P5a ✅), the largest *measured*
gap left is **per-call overhead on call-bound benches**. Root cause traced: every Chezzi call recurses
into a fresh Rust `run_until` loop (`Op::Call` → `do_call` → `run_proto_in_place`, `mod.rs:1992` →
`run_until`), so each call pays (1) a **native Rust stack frame** and (2) **`Arc::clone(&self.program)`**
(`mod.rs:2115`) — an atomic per call. fib(30) ≈ 2.7M calls ⇒ 2.7M recursions + 2.7M atomics. **That's
why `fib` is 3.85× CPython while `loop` is 1.31×: the gap is the *call*, not dispatch** (`primes` 2.50×
is also call-bound and would move). Fix = CPython 3.11's "zero-cost frames": bytecode `Op::Call` pushes
a frame and `continue`s the existing loop; `Op::Return` pops + pushes result + continues — no Rust
recursion, one `Arc::clone` per `run_until` not per call. **Parity risk:** pause/park (B1/D3), `recover:`
unwind, `defer` lean on Rust-stack unwinding — a flat loop must park by leaving `self.frames` intact and
breaking (M:N `FiberCtx` save/restore already does this). **Keep `run_proto_in_place` for native callers**
(HOFs need the callback result synchronously mid-method); only bytecode `Op::Call` flattens. Cheap
stand-alone warm-up: hoist the per-call `Arc::clone`. VM-only blast radius; parity testable against the
fib / recover-in-recursion / defer-in-recursion / deep-recursion-overflow goldens. Full write-up in
[`docs/future.md §4`](docs/future.md). **GC is NOT the lever** — share-nothing per-thread, moves no bench;
generational GC stays a low-priority separate milestone.

**Remaining concurrency work — Tier-D is complete; only M-C (implicit nurseries) is left, deferred.**
The concurrency roadmap landed through **Tier-D** (D0 ready-queue, D1 lazy module snapshot, D2a/D2b M:N
scheduler + parking fibers, D3 reduction-counting preemption, D4 work-stealing run queues, D5 dirty/
blocking pool, D6 netpoller + non-blocking `std.net`). There is **no Tier-E**. The single remaining item
is **M-C — implicit nurseries** (`docs/concurrency.md §10`): make every function body an implicit
nursery that joins at its `return`/end, dropping the explicit `parallel:` requirement. **Deferred** —
an invisible "joins at end of function" barrier hides *when* work runs, which matters while execution is
observable-sequential; "revisit after C5". Ergonomic sugar, not new capability.

**Robustness pass — cyclic-data depth guard + order-independent map `==` — has now landed** (both
engines). Two fuzzing-found bugs: (1) a cyclic data structure (a struct with a `list[Self]` field
forming a cycle) made `print`/`==` recurse unbounded on the **host** stack inside the value-display
and value-equality routines → uncatchable SIGABRT, even inside `recover:`; (2) map `==` was
order-*dependent* (positional zip) while set `==` was order-independent — inconsistent. Fix:
a contained recursion guard `MAX_STRUCTURAL_DEPTH = 10_000` threaded through display
(`stringify`/`display_guarded`) and a new `values_equal_guarded(..) -> Result<bool, RuntimeError>`
(the public `values_equal -> bool` stays a thin wrapper over it, so the ~66 hash-probe/`contains`
call sites are untouched and depth-exceeded degrades to "not equal"); the recoverable
`maximum structural depth (10000) exceeded (cyclic data structure?)` error is surfaced only at the
`==`/`!=` op sites. Map `==` is now order-independent value equality (same key→value pairs), mirroring
the Set arm. A 2-agent review caught + fixed two parity gaps before commit: interp `list.contains`/
`index_of` used derived `==` (unguarded → SIGABRT on cyclic data; now route through `values_equal`),
and the interp equality lacked the VM's identity fast-path (`a == a` on a self-cycle diverged; added
`Rc::ptr_eq` short-circuits). New goldens `examples/map_eq.chz` + `examples/cycle_guard.chz` (VM ==
interp == `.expected`). Decision: the interp's *call*-depth overflow in **debug** builds is left
as-is (the tree-walk engine is slated for removal; release is fine, VM is fine). Latest suite:
**1497 tests** green (unit + parity + `cargo test conformance`), `cargo clippy -- -D warnings` clean.

Core language is feature-complete through **M18** plus several gap-closing passes. Concurrency
**C1 + C2 + C3 + C4** have landed (both engines), plus the **`Executor` escape hatch** (C5's
sequential subset) with **program-exit auto-drain** (C5 / A2) and the C5 checker refinements. So
`spawn` / `parallel:` / `Channel[T]` / `Shared[T]` / `Executor` all run on **both** engines.
**Group B's B1 + B2 (cooperative fibers + blocking `recv`) have now landed on the VM engine**: a
`recv` on an empty channel suspends the running fiber and the scheduler resumes it when a sibling
`send`s, so mid-flight producer/consumer works (`examples/channel_block.chz`). **`Channel.try_recv()`
(A1) — the non-blocking poll — now ships on both engines** (`examples/try_recv.chz`). **B3.0 — the
wire-format airlock — has now landed** (VM): the task-airlock `deep_clone` is implemented as a
`WireValue` serialize → reconstruct round-trip (`src/vm/wire.rs` + `Vm::to_wire`/`from_wire`),
byte-identical to the old direct deep-copy. **B3.1 — cores out of the heap — has now landed** (VM):
`Channel`/`Shared`/`Executor` data moved out of the GC heap into `Arc<…Core>` holding `WireValue`
(`src/vm/core.rs`), so the heap keeps only an `Obj::X(Arc<…Core>)` handle and a crossed core is shared
(not copied). The airlock serializes at the core boundary now; `children()` was *rewritten* (not
dropped — single-thread cores still embed `Handle(GcRef)`s) to keep queued strings/closures rooted.
**B3.2 — `Arc<Program>` + isolated worker-VM construction — has now landed** (VM): `program: Rc<Program>`
→ `Arc<Program>` (read-only sharable across workers), plus `Vm::spawn_worker` / `Vm::run_task_isolated`
— build a fresh worker `Vm` with its **own heap**, wire-copy a `spawn`'d function/closure task's
args+captures IN (callee lowered to `ProtoId` + wire'd captures, never a parent-heap handle), run it
**synchronously** (no threads), and wire result + per-worker `out`/`stderr` back. Cross-heap safety is
**enforced** (`WireValue::has_handle` + `Vm::ensure_crossable`): a `str`/closure value crossing — which
would be a dangling `GcRef` in another heap — is a clean fault, not silent corruption; method tasks are
gated off (a worker's `module_objs` is empty). All `#[allow(dead_code)]` until B3.3's `--parallel`
wires it in (decision A keeps the cooperative engine the default through B3.2). Still single-thread,
behavior byte-identical. Latest suite: **1292 tests** green (1287 + 5 new B3.2 units: distinct-heap /
result+out / program-Arc-sharing / str-rejection / method-rejection; unit + parity + `cargo test
conformance`), `cargo clippy -- -D warnings` clean.

**B3.3a — `str` crosses the airlock by value — has now landed** (VM): an owned-bytes
`WireValue::Str(Box<str>)` arm replaces the by-reference `Handle` for `str` in `to_wire`/`from_wire`/
`display_wire`/`collect_core_gcrefs`, so a `str` (and any data containing it) now crosses a worker
boundary instead of being rejected as a dangling `GcRef`. Parity-safe — `str` is immutable, value-
compared, has no identity operator — so a fresh handle on reconstruction is unobservable; all goldens
byte-identical.

**B3.3b — the G1 module-globals checker gate — has now landed** (checker): a reassignment of a module
global reachable, directly or transitively through free-function calls, from a `spawn` task is a type
error (*"cannot mutate module global '…' from a parallel task; use Shared[T]"*). Flow-scoped to spawn
reachability; scope-aware name resolution (params/`let`/`for`/`match`/closure/comprehension binders, so
a local shadowing a free fn or global is never mis-flagged); descends `recover:` blocks. Direct in-
`spawn:`-block writes stay caught by the existing `is_captured` gate. Reviewed by a 4-agent S++ panel
+ a cold pass (caught and fixed a false-positive on shadowed spawn targets and a `recover:`-block
false-negative before they shipped). Two indirect-dispatch gaps documented (global-closure spawn
target, method chains) → B3.3-threads. Latest suite: **1306 tests** green (unit + parity + `cargo test
conformance`), `cargo clippy` clean.

**B3.3c + B3.3d — worker module-graph reconstruction — have now landed** (VM, single-thread, parity-
preserved): the two remaining B3.3 "owes". **B3.3c (read-only `home` snapshot):** `Vm::build_worker_modules`
snapshots the parent's initialized `module_objs` into the worker heap (two-pass — alloc module objs,
then map globals), so a spawned task can read post-init module globals and call sibling/imported free
functions. It **snapshots, never re-inits** (re-running a toplevel would duplicate prints/I/O). The map
is the load-bearing GcRef-safety boundary: `map_global_value` rebuilds every `Func`/`Closure`/`Module`/
`Native` explicitly over the worker's home and **recurses structurally through containers**, so a
`[fn …]` handler list or `{k: fn …}` dispatch map cannot smuggle a parent-heap `GcRef` into the worker
(pinned by `worker_calls_through_global_fn_container`); only pure data + `Channel`/`Shared`/`Executor`
cores take the exact wire round-trip. **B3.3d (method tasks):** `run_task_isolated` lowers `spawn obj.m()`
to `Lowered::Method` (recv + args by wire) and dispatches via `do_method_call` against the rebuilt
`module_objs`; a method that blocks on `recv` faults cleanly (no scheduler in a sync worker). Still
`#[allow(dead_code)]`/test-only until the `--parallel` flip wires it onto threads. Latest suite:
**1312 tests** green (unit + parity + `cargo test conformance`), `cargo clippy` clean. Reviewed by a
2-agent panel (caught + fixed a container-of-callables GcRef-smuggle and a method-suspend pop underflow
before they shipped).

**B3.3-threads — real OS threads behind `--parallel` — has now landed** (VM): the thread-flip. A new
`--parallel` flag (`chezzi run --parallel`, VM-only; the cooperative single-thread engine stays the
**default** per decision A) sets `Vm.parallel`, switching `join_nursery` to `run_parallel_nursery`,
which runs a nursery's tasks on a **bounded OS-thread pool** (`src/vm/pool.rs` — one process-wide
pool of `available_parallelism()` threads, each with the 256 MiB VM stack). The joining thread runs
`tasks[0]` inline (decision B — **parent participates**, so nested `parallel:` never explodes the
thread count) and farms the rest to the pool; results join, each worker's `out`/`stderr` flushes in
**task order** (decision F — deterministic despite concurrency), and the first fault propagates.
`run_task_isolated` was split into `prepare_worker` (parent-heap half) + `ReadyWorker::run`
(thread-side half) — the prepared worker `Vm` **moves** onto a pool thread (`Vm` is `Send`: plain
data + `fn` pointers + `Arc<…Core>`, proven by a 2-thread unit test). A blocking `recv` under
`--parallel` waits on a real `ChannelCore` **condvar** (`send` wakes it) instead of parking a fiber;
**`Shared.update` now takes a per-core `update_lock` under `--parallel`** so concurrent
read-modify-writes can't lose each other (a lost-update race the first cross-thread golden caught —
`Shared[T]`'s whole contract is serialised writes). Worker `host` inherits the parent's read-only
args+env (stdin stays inert — a consumable stream isn't shared). Deterministic-by-construction
goldens: `examples/parallel_shared.chz` (N threads bump one `Shared` → exact count) and
`parallel_channel.chz` (a collector recv-blocks across threads, sorts → fixed order). Every existing
golden + the 3-way VM==interp parity stays on the default engine, **byte-identical green**. Latest
suite: **1319 tests** green (unit + parity + `cargo test conformance`), `cargo clippy` clean.
**Still owed (later phases):** `Executor` doesn't yet ride the pool + the A3b `submit`-capture gate
(B3.6).

**B3.4 — cancellation + cross-thread `os.exit` — has now landed** (VM, `--parallel`). Each worker
`Vm` carries a per-nursery `cancel: Arc<AtomicBool>` (cloned in by `run_parallel_nursery`) plus a
`cancelled` latch. `ReadyWorker::run_outcome` classifies each task into a `TaskOutcome`
(`Done`/`Cancelled`/`Exit{code}`/`Fault`); the **first sibling to fault or `os.exit` trips the flag**
(`Vm::trip_cancel`), and the join scans outcomes in task order — flushing `Done`/`Exit` output and
propagating the lowest-index `Exit` (→ parent `pending_exit`, a hard halt with the child's code) or
`Fault` (normal unwind, so an outer `recover:` still catches it). Running siblings observe the flag
at the **dispatch back-edge** (`run_until` loop top, beside the `gc_stress` check, gated by
`!self.cancelled` so a cancelled task's `defer`s still run) and a **`recv` `wait_timeout`
re-checking loop** (50ms) — the latter chosen over a separate cancel condvar because the faulting
worker can't know which channel cores siblings park on; the bounded re-check eliminates the
lost-wakeup hazard (risk #2) at a ≤50ms abort-latency cost. So the first child fault now **aborts
running siblings** (a recv-blocked sibling whose producer faults no longer hangs the join), and a
child `std.os.exit(code)` halts the whole process cross-thread with the right code. `recover:` /
`defer` compose — crucially, the cancel sentinel **bypasses `recover:`** (a cancelled task must die,
not resume) while still running `defer`s via `unwind_deferred`, on *both* the back-edge and recv
paths. An `os.exit` **wins over** any sibling fault regardless of index (a hard halt is never demoted
to a catchable error). New: `examples/parallel_cancel.chz`. Reviewed by a 2-agent concurrency/safety
panel: caught + fixed three real defects before commit — (1) the cancel sentinel was catchable by a
worker-internal `recover:` and skipped `defer`s on the CPU path; (2) `os.exit`-vs-fault precedence;
(3) an `Arc::try_unwrap` join race (a finished pool thread still holding a `results` clone) → now
`mem::take` under the lock. Latest suite: **1328 tests** green (unit + parity + `cargo test
conformance`), `cargo clippy` clean. Single-level cancel only — nested-nursery cancel propagation is
a documented, deferred limitation (`docs/concurrency-b3.md`).

**B3.5 — nursery-local deadlock detection under threads — has now landed** (VM, `--parallel`). Under
B3.4 a genuinely all-blocked nursery *hung* (the `recv` re-check only aborted on *cancel*); now each
worker `Vm` also shares a per-nursery `DeadlockWatch` (`Mutex<{blocked, live, epoch, confirms,
dead}>`, cloned in by `run_parallel_nursery` like the cancel flag). A blocking `recv` runs a
**barrier-confirm** detector (decision D): a parked worker "confirms empty" only when every still-live
sibling is parked (`blocked == live`) and does so at most once per `epoch`; any progress — a `send`,
a successful pop, a park-count change, or a task finishing (`task_finished` decrements `live`) — bumps
`epoch` and resets `confirms`. When `confirms == live`, every live worker independently re-checked its
own channel empty in the *same* epoch with no intervening progress ⇒ no message exists and no sibling
can send ⇒ fault `deadlock` (the **byte-identical** message the cooperative scheduler uses, now a
shared `DEADLOCK_MSG` const). This is immune to the "message delivered, consumer hasn't popped yet"
false-positive a plain blocked-count detector hits: a worker holding a deliverable message pops it
instead of confirming. **Lock discipline:** the watch mutex and a channel `q` mutex are never held
simultaneously (each recv phase takes one lock at a time; `send` bumps the epoch then releases before
pushing) — no lock-order cycle. Soundness rests on a `--parallel` nursery being the only thing running
(the parent thread is inside `run_parallel_nursery`), so its own live tasks are the only possible
senders. Five new tests (the cooperative all-blocked golden ported to `--parallel`, a near-miss + a
3-task chained relay that must NOT false-positive, a finished-task-strands-sibling case, all behind a
5s watchdog so a regression fails loudly instead of hanging) + `examples/parallel_deadlock.chz`.
Reviewed by a 2-agent concurrency panel (Solidity + SRE): detector logic confirmed sound (lock
ordering, no false-positive across stress runs, no missed-wakeup, counter integrity, poison-tolerant);
documented the residual hangs (Go-like, decision D) — deadlocks spanning nurseries / involving
`Executor`, an orphaned message no live sibling reads, and the **G3 saturated-pool** case (a sibling
still *queued* counts toward `live` but never parks, so the nursery waits for a slot rather than
faulting — counting a queued task as live is the deliberate no-false-positive choice). Latest suite:
**1332 tests** green (unit + parity + `cargo test conformance`), `cargo clippy` clean.

**B3 is decomposed into a persistent, multi-session plan.** Tier-C OS-thread multicore (B3) — with
B4 (real `Shared`) and B5 (real `Executor` pool) folded in, since under shared-nothing threads they're
the same machinery — is broken into seven TDD phases **B3.0…B3.6** in
**[`docs/concurrency-b3.md`](docs/concurrency-b3.md)** (validated shared-nothing architecture,
decisions A–G, risk register, per-phase TDD focus). The surface of `spawn` / `parallel:` / `Channel` /
`Shared` / `Executor` stays **unchanged**.

**B3.6 — `Executor` on the pool + the A3b `submit`-capture sendability gate — has now landed** (VM +
checker, `--parallel`). **A3b (checker):** `Executor.submit`'s closure runs on a pool thread, so its
captures cross the airlock exactly like a `spawn` task's — the `Ty::Executor` `submit` arm now pushes a
`capture_floor` (at the current scope depth) around the argument check, so the pre-existing
`infer_ident` read gate flags a non-sendable captured binding (a `Ref`, a function-local closure) while
the closure's own params/locals stay task-local. **VM:** a new `WireValue::Closure { proto, captured,
home }` arm crosses a submitted closure **by value** (proto via the shared `Arc<Program>`, captures
wired recursively, `home` as a `module_objs` index — no heap-local `GcRef`); `Vm::wire_callable`
produces it at `submit` **only under `--parallel`** — the cooperative default engine keeps crossing the
closure **by handle** (`to_wire` → `Handle`) so its drain on the same heap shares captures by reference
(a mutation between `submit` and drain stays observable, matching the interp oracle — a by-value snapshot
would break `VM == interp` for the sequential subset, decision A; caught in review). `from_wire` rebuilds
the `Closure` over the worker's reconstructed home, and `collect_core_gcrefs`/`has_handle`/`display_wire`
gained matching arms. Under
`--parallel`, `shutdown` (and the program-exit autodrain, which calls it) drains the whole queue under
the core lock then farms the tasks to the bounded pool via a new engine-agnostic
`run_workers_on_pool` (extracted from `run_parallel_nursery` — the nursery and executor drains now
share one farm/join/flush core); each executor task gets a fresh per-drain cancel flag (first fault
aborts siblings, matching the cooperative inline `r?`) but **no** `DeadlockWatch` (decision D — an
`Executor`-spanning deadlock is an accepted hang). Cooperative drain stays inline and byte-identical
(decision A oracle). New: `examples/executor_pool.chz` (submit→pool-drain→sort, same output on both
engines); tests `golden_executor_pool_chz_matches_expected`, `executor_submitted_closure_captures_by_value`,
`executor_cooperative_submit_shares_captures_by_reference` (the decision-A regression pin), and six
checker A3b tests (`submit_{non_sendable_capture,captured_closure,captured_closure_through_nested_closure}_rejected`,
`submit_captured_{channel,int}_ok`, `top_level_closure_submitted_ok`). Latest suite: **1341 tests** green
(unit + parity + `cargo test conformance`), `cargo clippy` clean. Reviewed by a 2-agent panel
(concurrency/VM + checker); the C-01 cooperative-snapshot regression it caught is fixed + pinned.

**With B3.6 landed, the B3 epic (B3.0…B3.6) is complete** — `spawn` / `parallel:` / `Channel` /
`Shared` / `Executor` all run on real OS threads behind `--parallel`, surface unchanged. **Next
frontier:** **Tier-D** (M:N scheduler + async-I/O pollset), designed in
**[`docs/concurrency.md` §10](docs/concurrency.md)** and now **broken down into seven TDD phases
D0…D6** in **[`docs/concurrency-tier-d.md`](docs/concurrency-tier-d.md)** — Go-style GMP work-stealing
skeleton + BEAM-style reduction-counting preemption & dirty pool for opaque blocking native calls
(full Go-vs-BEAM borrow ledger in that file). **D0 has landed** — the cooperative scheduler's
O(N²) per-turn linear scan (`pick_runnable`) is replaced by an explicit per-nursery ready-set
(O(log N)/turn), so 50k cooperative fibers run in ~tens of ms instead of seconds. **D1's
lazy-module-snapshot half has landed** (see below). **D2a has landed** — D1's deferred other half:
`Heap` is now part of the swappable `FiberCtx` as `heap: Option<Heap>`, swapped only for M:N fibers
(`Some`); cooperative fibers carry `None` and keep aliasing the single `Vm::heap` (decision A —
share-by-ref), so the engine stays byte-identical by construction. D2a was the parity-preserving prerequisite that made
a `Fiber` self-contained + `Send` so D2b could park it across worker threads. **D2b has landed** —
the `--parallel` engine is now a true M:N scheduler: lightweight fibers (own heap, share-nothing)
multiplexed over the bounded pool, **parking on `recv` instead of pinning OS threads**, so a
`#fibers ≫ #threads` producer/consumer workload completes instead of starving (1000 consumers +
1000 producers in ~0.02 s). One shared per-nursery run queue + park set (`MnSched`); `send` enqueues
and re-queues parked waiters atomically (lost-wakeup-safe); deadlock is the exact predicate
`running==0 && runq empty && parked>0 && done<total`; the joining thread runs an inline shell that
alone guarantees completion (decision B), so the join never waits on a bounded pool resource (no
nested/concurrent pool-exhaustion deadlock). The legacy condvar `recv` + `DeadlockWatch`
barrier-confirm detector were retired. Reviewed by a 4-agent S++ panel + cold pass — two Criticals
found (a defer-on-cancel test race and a nested pool-exhaustion join hang) and both fixed. **D3 has
landed** — **BEAM-style reduction-counting preemption**: a fiber carries a reduction budget
`reds: u32` (reset to `CONTEXT_REDS = 4000` per schedule-in); the `run_until` loop-top safepoint
decrements it per op under the M:N engine and, at exhaustion (`native_reentry == 0`), **yields** —
stops dispatch and requeues the fiber at the **tail** of the shared run queue (`Disp::Yield` →
`MnSched::yield_fiber`, round-robin), so a CPU-bound fiber can no longer hog its worker while
siblings starve (64 spinning hogs ≫ pool that would hang without preemption now complete). The yield
reuses the recv-park suspend/rewind contract, so it unwinds every nested `run_until` level via a
`paused()` helper (`suspend.is_some() || yield_now`) at each propagate-up gate — the fix for a found
bug where a yield deep in a call chain let `run_proto` pop a live operand-stack temp
(`expected bool, found int` on `primes_parallel`). Cooperative engine byte-identical by construction
(`yield_now` gated on `mn.is_some()`). **1365 tests** green (+4: fairness hang-watchdog, 10 k-fiber
soundness churn, nested-call unwind regression, `yield_fiber` unit), `cargo clippy` clean,
`primes_parallel=148933` both engines, all `--parallel` goldens byte-identical; 4-agent S++ backend
panel (Godot Gameplay / Solidity / Incident Response / SRE) — zero real findings.
**D4's work-stealing half (D4a–D4d) has landed** — per-worker local run queues (`LocalQ` =
`runnext` + ring, lock class B) + a shared `global` overflow queue, a capped global batch-grab
(`globrunqget`), random-victim steal-half (`try_steal`), and a periodic global check (`tick%61`),
replacing the D2b single shared run queue. The deadlock predicate now reads a `runnable: AtomicUsize`
(count of fibers queued anywhere) instead of `runq.is_empty()`. `yield`→global (fairness, so a CPU
hog can't re-pop its own local forever); only the batch-grab populates locals; stealing rebalances.
Lock order strictly B-then-A / A-then-C → no ABBA.
**D4e (the wake protocol) has landed — as a runnable-gated park, NOT Go's `nmspinning` + SeqCst
StoreLoad fence.** The `cv.wait_timeout(2ms)` poll is gone: `take_runnable`'s park branch now does a
**true `cv.wait`** (no timeout, woken only by a sibling's `notify`) when `runnable == 0`, and
**re-steals after a brief bounded `cv.wait_timeout(SPIN_BACKOFF=500µs)` backoff** when `runnable > 0`
(work sits in a local — stealable — or in the sub-µs in-hand `Vec` window of a concurrent grab/steal;
the backoff is cut short by any wake `notify_all`, so it adds no hot-path latency — it only stops the
idle workers from busy-spinning on the core lock across that window). **Why not the
Go fence:** Go needs the lockless StoreLoad barrier only because it lacks a global runnable counter;
chezzi's `runnable` atomic is mutated under the core lock at every enqueue and read under that same
lock right before `cv.wait`, so the **mutex *is* the StoreLoad barrier** — lost-wakeup-free by the
standard locked-condvar argument, simpler and easier to prove, no new atomics/fence/park primitives.
The in-hand `Vec` window (counted-but-momentarily-unreachable) is a bounded handful of `VecDeque`
pushes by a non-blocked worker, so the spin is bounded, not a livelock; a `debug_assert!(runnable==0)`
before `cv.wait` pins the invariant. **Deferred (optional, throughput-only):** the conditioned
single-wake (`notify_one` + idle-count) that would avoid the `notify_all` thundering herd — pure
efficiency, correctness-irrelevant, to add only if a benchmark justifies it (and where a `cfg(loom)`
model would then earn its keep). +2 tests: `d4e_pingpong_no_lost_wakeup_stress` (×25-round watchdog
lost-wakeup guard), `d4e_wake_parked_workers_from_true_sleep` (wake-from-`runnable==0`-sleep). 1386
tests green, clippy clean, `primes_parallel=148933` both engines, goldens byte-identical, release
stress ×4 stable. 4-agent S++ concurrency panel (Godot Gameplay / Solidity / Incident Response /
SRE): zero Critical, zero lost-wakeup/hang; applied SRE's two Importants (the `runnable>0` busy-spin
→ bounded `wait_timeout` backoff, killing a thundering-herd / oversubscription-starvation regression;
+ corrected a stale `runnable` doc-comment that claimed an out-of-lock mutator, on which the gate's
soundness depends). **D4 epic complete.**
**D5 (dirty/blocking pool) has landed** — a blocking native call (`std.io.read_file`/`write_file`,
`std.fs.*`, `std.time.sleep_ms`) no longer pins a core worker (the **live G3 starvation is fixed**).
At its dispatch site (`invoke_native`, gated on `mn.is_some() && native_reentry == 0`) a blocking,
*off-heap-safe* native (`native::is_blocking`) is intercepted: its args are materialized into `Send`
primitives (`NativeArg`), the fiber suspends like a `recv`-park (`Vm::offload` + the `paused()`
push-skip gate), and the worker hands it (`Disp::Offload`) to a **growable blocking pool**
(`src/vm/blocking_pool.rs`: spawn-on-stall, reap idle past 10 s, cap 512) that runs the native with no
`Vm`/heap (`OffloadHost`, host-I/O methods `unreachable!`). On completion the pool stashes the raw
`NativeRet` on the fiber and `complete_offload`s it back onto the global queue + `notify_all`; the
resuming worker lowers + pushes the result and continues past the `Call`. A 4th fiber state,
`MnSched.inflight`, is added to the deadlock predicate (`is_deadlocked`) so an in-flight blocking call
vetoes a false deadlock fire. A panic in an offloaded native is caught in the pool job and surfaced as
a task fault (never a lost fiber / pinned `inflight`). `sleep_ms` rides the same pool (so `sleep_ms`
×N runs concurrently, ≈ max not sum); a blocking native reached *inside a native callback*
(`native_reentry > 0`) still runs inline. Cooperative/`--interp` byte-identical (offload is M:N-only).
**1384 tests** green (+12: `is_blocking` ×2, `blocking_pool` ×4, `offload`/`is_deadlocked`/panic ×3,
`d5_*` program ×3), `cargo clippy -- -D warnings` clean, `primes_parallel=148933`, sleep+fs program
byte-identical across `--interp`/`--parallel`/default. 2-agent S++ panel (SRE + VM/invariant): one
Critical applied (panic-in-offload → pinned `inflight` hang; now caught + faulted), one Important
applied (`submit` `notify_all` not `notify_one`, closing a reap-vs-wake race).
**D5 owes #1 + #2 have landed** (this session). **Owe #1** — `std.request` (`get`/`post`, HTTP via
`ureq`) and `std.process` (`cmd`, subprocess) are now classified blocking-offloadable (added to
`native::is_blocking`): both verified off-heap-safe (primitive `str` args, primitive `Struct`/`Ok`/
`Err` returns, no heap/stdio touch during the call — they run on the `OffloadHost`), so network /
subprocess I/O no longer pins a core worker. **Owe #2** — a process-wide **timer thread**
(`src/vm/timer.rs`: a deadline min-heap + one thread, lazy `OnceLock`) replaces the one-blocking-pool-
thread-per-sleep model: `sleep_ms(N)` now parks the fiber on the timer (`OffloadReq.timer_ms`
branches `MnSched::offload` to `timer::submit_at` instead of the dirty pool), waking it at the
deadline via the same `inflight`→`complete_offload`→`notify_all` path (so the deadlock predicate
stays sound — a sleeping fiber is `inflight` and vetoes a false deadlock). 10⁴ sleepers ≈ 1 thread,
not 10⁴; `sleep_ms(<=0)` runs inline (no park); a pathological `ms` saturates via `checked_add` (no
`Instant`-overflow worker panic). **+7 tests** (`is_blocking` request/process ×1, member-name-unique
guard ×1, `timer` unit ×3, `timer_offload` park ×1, `d5_owe1` process.cmd program ×1 — **1393 green**),
`cargo clippy --all-targets -- -D warnings` clean, `primes_parallel=148933` (VM + `--parallel`),
VM==interp parity suite green, `sleep_ms` fan-out runs ~max not sum (timer path). 2-agent S++ panel
(SRE + Backend Architect): zero Critical; both Importants applied (timer-deadline `checked_add`
saturation; bare-name-collision guard test).
**D5 owe #3 — Path A has landed** (this session). The `recv`-inside-callback unblock for the
iteration HOFs: `map` / `filter` / `fold` / `reduce` are now **chezzi source** in `std/iter.chz`
(beside the pre-existing `enumerate` / `zip`). Reached through `iter.map(xs, f)` the per-element
callback runs entirely through **VM frames** (no native Rust loop frame in the chain), so a blocking
`recv` (or `sleep` / socket op) inside the closure **parks** under `--parallel` instead of faulting
`deadlock` — the BEAM `Enum.map`-over-a-NIF split, zero Rust runtime change. **Generic-return
inference** binds `U` (and `fold`'s `A`) **from the closure alone** (not from `xs`) — the flagged
risk — and works with **no explicit type args**. The native builtin `xs.map(f)` is **kept** as the
faster non-blocking path (documented: *use `iter.map` if the callback may block under `--parallel`*).
`each` deferred — a void fn-type param `fn(T)` doesn't parse yet (grammar requires a `->` return; use
a bare `for x in xs:`). **+2 tests** (`d5_owe3_recv_in_iter_map_callback_parks` — recv-in-closure
across a nursery sums `66`, 30 s watchdog vs hang; `d5_owe3_iter_hofs_correct_on_both_engines` —
map/filter/fold/reduce byte-identical VM/interp incl. `int -> str` map). **1412 green**, `cargo
clippy --all-targets -- -D warnings` clean, conformance green, `enumerate`/`zip` users
(`examples/for_tuple.chz`) unchanged. **Remaining owe #3 — Path C residual** (Go-`handoffp`
thread-demote for the intrinsically-native islands: `Shared.update`'s lock, hash/compare/str hooks,
fast native `sort`) — only when real programs hit the wall; Path B (stackful) rejected. **2-agent S++
panel** (SRE + Backend Architect): zero Critical; applied SRE's Important (the park test's parallel
leg can't *force* a park over an unbounded FIFO — added a **deterministic cooperative leg** that parks
before the producer can run, so a park/wake regression faults/hangs instead of flake-passing) + SRE's
Minor (non-commutative `fold` subtraction locks left-to-right). **Known follow-up (both reviewers, not
this PR):** a user who reaches for the native `xs.map` with a blocking callback hits the generic
`deadlock` fault, which names channel topology, not the `xs.map`→`iter.map` fix — a native-callback-
specific fault message (`src/vm/mod.rs` guard sites) would make the footgun self-correcting.
**D5 owe #3 — Path C has landed** (this session, an *attempt*; branch `d5-owe3-path-c`). The
intrinsically-native islands `iter.*` can't move to (the native `xs.map`/sort comparator/`Shared.update`
callback) now **demote the worker thread** instead of faulting `deadlock`: a blocking `recv` reached with
`native_reentry > 0` under `--parallel` (host-stack loop frame, unsnapshotable) calls a new
`Vm::demote_recv_block` — it accounts the fiber as a 5th state `blocked_native` (running→blocked_native
under the core lock + `cv.notify_all`), spins up **one raw replacement OS thread** (`spawn_replacement_worker`,
reusing the worker's `wid`; a fresh thread, NOT a pool job — the pool is fixed-size), and **blocks in place**
on the channel's own condvar (`ChannelCore.cv`, revived from B3.3), resuming in place when a sibling
`send`s (`send_wake` now also `core.cv.notify_all`s). After the fiber settles, `mn_worker_loop` returns
(`if self.demoted`) so the demoted thread exits — net-zero worker count, Go's `handoffp` cost (+1 OS
thread per fiber *actually* blocked in a callback). `is_deadlocked` gains `|| blocked_native > 0` so a
genuine in-callback deadlock still **faults** (the demote loop self-evaluates the predicate + `flag_deadlock`,
so detection doesn't depend on a separate puller being alive); the block loop checks the queue **before**
`terminate` so a real send always wins. The joining thread `wait_for_completion`s (`done == total`) before
the slot reduce (the demoted/early-exited loop can return before all slots fill). **Pragmatic scope** (user
decision): the one narrow false-positive — parked siblings spuriously faulted when a value is queued for
*another* demoted fiber while `running == 0` — is documented, not closed. **+2 tests**
(`d5_owe3_path_c_recv_in_native_map_callback_demotes` — recv inside native `xs.map`, producer `sleep_ms`s
to force the empty recv, sums `66` under a 30 s watchdog; `d5_owe3_path_c_recv_in_callback_no_sender_still_deadlocks`
— no-sender recv-in-callback faults `deadlock`, not hang). **1417 green**, `cargo clippy` clean, conformance
green; the cooperative fault pins (`fibers_recv_inside_map_callback_faults`/`_index_overload_`/`_defer_`)
unchanged (Path C is M:N-only) and `d5_blocking_native_in_callback_runs_inline` (sleep-in-callback stays
inline — demotion is scoped to `recv`) stays green. **2-agent S++ panel** (concurrency + quality): zero
Critical; both Importants applied — spawn-failure (OS thread limit) now faults the fiber cleanly instead of
panicking mid-accounting; the demote loop self-detects deadlock so it can't hang as the last live worker.
**Residuals (documented):** the narrow parked-sibling false-positive; the `Shared.update` same-box hazard
(a `recv` blocking inside `update(f)` holds `update_lock` — *don't block on a value needing the same box*);
demotion scoped to `recv` (a `sleep_ms`/socket op inside a callback keeps its current path); Path B
(stackful) still rejected.
**D6a (netpoller + non-blocking `std.net` TCP surface) has landed** — the epoll/kqueue netpoller
(`src/vm/poller.rs`, via the `polling` crate: one process-wide poll thread + an fd→parked-fiber
registry) turns a would-block socket op into a cheap fiber-park instead of a pinned worker. `std.net`
adds `connect`/`listen`/`accept`/`read`/`write`/`close`/`addr` over a new heap object kind
`Obj::Socket(Arc<SocketCore>)` / `Obj::Listener(Arc<ListenerCore>)` — structurally a `Channel` (an
`Arc`'d core outside every heap, a `WireValue` arm so a handle crosses to a spawned fiber, GC tracing
that roots nothing). Sockets are **non-blocking**; on `WouldBlock` the op rewinds `ip` + re-roots its
receiver/args (mirroring the `recv`-park, but re-pushing args too) and sets a new `Disp::PollPark`,
which `MnSched::poll_park_offload` accounts as **`inflight`** (running→inflight) before registering fd
interest. The poll thread injects the fiber back on OS readiness via the **existing**
`complete_offload` (inflight→runnable + `notify_all`) — so the op re-runs and the deadlock predicate is
unchanged (an in-flight socket op vetoes a false deadlock; a lone `accept`-parked server with no client
correctly never self-terminates, Go-identical). `connect`/`listen` are intercepted in `invoke_native`
(they allocate a heap handle, not an off-heap native); `read`/`write`/`accept`/`close`/`addr` dispatch
inline like `channel_method`. The checker gains `Ty::Socket`/`Ty::Listener` (sendable, non-generic,
the runtime↔checker↔native lockstep maintained). **Headline:** an echo server services **100
connections ≫ core workers** in one `parallel:` (`net_echo_server_services_more_conns_than_workers`)
and `examples/echo_server.chz` runs; without the poller the bounded pool would starve. fd lifecycle is
delete-before-drop on every path (`close` de-registers before dropping the stream; `Option<TcpStream>`
makes use-after-close a clean fault). **2-agent S++ review panel (Security Engineer + Code Reviewer):
one Critical applied** — two fibers sharing one socket `Arc` and both reaching a would-block op would
overwrite the per-fd poller registration (drop the first fiber + leak `inflight`) and double-`add` the
fd (`EEXIST`-panic the poll thread); now a per-`SocketCore` `in_flight` guard (set on park, cleared by
the poller on inject so the owner can re-park) makes a concurrent shared-socket op fault cleanly. **Two
Importants applied** — the non-`--parallel` would-block path now fails loud (`Result::Err`, net needs
`--parallel`) instead of blocking the only thread (a silent hang that also defeated the cooperative
deadlock detector), and `read(n)` caps its buffer at 16 MiB (`MAX_SOCKET_READ`) against a
caller-controlled OOM. **1404 tests green** (+11: socket core ×1, poller unit ×4, `poll_park_offload`
×1, `net.rs` helpers ×3, loopback round-trip ×1, echo-server ×1), `cargo clippy` clean,
`primes_parallel=148933` (VM + `--parallel`), all `--parallel` goldens byte-identical.
**D6b has landed — the D-tier is complete through D6.** Three follow-ups closed the D6a gaps:
**(1) Drain-on-fault (the hard gate).** `poller::drain_sched(&sched)` re-injects every fiber parked on
that nursery's sockets; `mn_worker_loop`'s abort branch now calls it beside `cancel_drain` (which only
walks the channel-`recv` `parked` buckets). A re-injected fiber resumes and hits the cancel check at
`run_until`'s loop-top **before** its rewound socket op re-runs, so it unwinds as `cancelled` and the
fault propagates — a net server may now share a nursery with a fallible sibling instead of **hanging
the join** (the previous documented hard gate). **(2) Timer fold.** The dedicated `sleep_ms` timer
thread is gone: the netpoller's poll thread now owns the timer min-heap, `wait()`s with a
deadline-bounded timeout, and fires due timers on wake (`submit_timer` + `poller.notify()`); `timer.rs`
is a 2-line shim over `poller::submit_timer`. One OS thread serves both socket readiness and sleeps.
**(3) True non-blocking `connect`.** `socket2`-based: a non-blocking connect that returns `EINPROGRESS`
parks the fiber on **writability** (a fresh `Disp::PollPark` with `pending_connect` — the connecting
`TcpStream` stashed in `FiberCtx`, swapped per-fiber, non-heap so no GC rooting); on writability the
poller injects it and `run_one_fiber` completes via `finish_connect` (`SO_ERROR`), pushing the `Socket`
with **no `ip` rewind** (the call already advanced). The loopback fast path still returns synchronously;
the cooperative/top-level fallback blocks with a 10 s wall-clock cap (`CONNECT_BLOCK_TIMEOUT_SECS`),
and a `connect` inside a native callback fails loud like `read`/`write`. A **register-vs-cancel race**
surfaced and was fixed by serializing register/deregister/`drain_sched`/fire-path (incl. the fd
`add`/`delete`) under the registry lock, with `register` rejecting (returning the fiber to re-inject)
when cancel is already set. **2-agent S++ review panel (Code Reviewer ×2): no Critical; two Importants
applied** — the top-level blocking connect is now bounded (was an unbounded spin on a black-hole
address), and `connect` inside a native callback fails loud rather than pinning a worker. **1410 tests
green** (+drain unit, +timer fold ×4, +3 net VM tests, +1 net unit), `cargo clippy` clean, full
`--parallel` net suite + the hang-regression watchdog tests pass, `examples/echo_server.chz` serves 50
conns. Items *not* in B3–B5 (cross-nursery wakeups, recv-in-native-callback / D5 owe #3,
~~`Channel.close()`~~ [landed], ~~per-connection `spawn`~~ [landed — eager injectable nursery, see below], per-socket read/accept timeout) are documented in
**[`docs/concurrency.md` §11](docs/concurrency.md)** and **[`docs/concurrency-tier-d.md`](docs/concurrency-tier-d.md)**.
Full A/B breakdown: §9.

**D5 owe #3 — Path C residuals #1 + ALL of #3 have landed** (branch `d5-owe3-path-c`), leaving only #2
(WON'T FIX by design). #1 + #3-sleep landed 2026-06-11; **#3-socket landed 2026-06-12** (see below + 
`docs/concurrency-tier-d.md`). **#1 (deadlock false-positive, a correctness bug):**
when 2+ fibers were demoted and one had a value already queued, `is_deadlocked` could fault an innocent
**parked sibling** with a fake `deadlock` (the demoted fiber polls its OWN channel queue — a `send`
`push_back`s + notifies `core.cv`, it does NOT bump `runnable` — so a queued value was invisible to the
counter-only predicate). Fixed: `SchedCore` now registers each demoted fiber's `Arc<ChannelCore>`
(refcounted `demoted_chans` map, `register/unregister_demoted`); `is_deadlocked` peeks the registered
queues under core lock A (A-then-q, the `send_wake` order) and vetoes the fire if any is non-empty (that
fiber will pop + progress); and `demote_recv_block` was restructured so `pop + blocked_native-- +
un-register` is **atomic under core lock A** — the checker never observes an emptied-but-still-counted
demoted fiber. **#3-sleep (perf/liveness):** a `sleep_ms(ms>0)` reached inside a native callback
(`native_reentry > 0`) used to run **inline**, pinning the worker; it now calls a new
`Vm::demote_block_sleep` — `spawn_replacement_worker` + `thread::sleep(ms)` in place + resume, accounted
`running→inflight` (so a sleeper **vetoes** deadlock — it returns unconditionally, unlike a
`blocked_native` recv); a cancel observed during/after the sleep swallows the task outcome (mirrors the
recv demote, so a cancelled task stops sleeping through the rest of the callback). **#3-socket
(2026-06-12, perf/liveness):** a socket `read`/`write`/`accept` that `WouldBlock`s inside a native
callback (`native_reentry > 0`) used to surface a misleading `"… require the --parallel engine"` error
(`park_on_fd` only snapshot-parks at `native_reentry == 0`; the callback's Rust-stack loop can't park).
It now demotes via new `Vm::demote_block_socket`: `demote_socket_enter` (`running→inflight` + spin a
replacement once, reusing `self.demoted`) → the worker **kernel-blocks on the fd** with `wait_fd_ready`
(`libc::poll` in the read/write direction, `DEMOTE_POLL_BACKOFF` timeout — woken on readiness, no
busy-poll) and re-runs the non-blocking op via a `SockPoll`-returning closure → `demote_socket_exit`
(`inflight→running`). Accounted `inflight` for netpoller-park parity (vetoes deadlock; a lone in-callback
`accept` with no client never self-terminates, Go-identical). `connect`-in-callback left unchanged
(handshake state in `pending_connect`, rarer). **+6 tests total this branch** — #1: `deadlock_predicate_
vetoed_by_queued_value_on_demoted_channel`, `..._refcounted_for_two_fibers_on_one_channel`,
`d5_owe3_path_c_no_false_deadlock_when_demoted_fiber_has_queued_value` (200× race stress); #3-sleep:
`d5_owe3_path_c_sleep_in_callback_demotes_frees_worker` (N=workers·4 <450ms), `..._sleep_in_callback_correct`;
#3-socket: `d5_owe3_path_c_socket_read_in_callback_demotes`, `d5_owe3_path_c_accept_in_callback_demotes`.
**1424 green**, `cargo clippy` clean, conformance green, `echo_server.chz --parallel` e2e green.
**4-agent S++ panel (Godot Gameplay, Solidity, Incident Response, SRE) on the socket change:** Critical
applied — poison-tolerant socket-core locks in the demote closures (a concurrent-`close` poison could
have skipped the `inflight` restore → permanent `inflight` leak → nursery deadlock detector wedged);
Important applied — cancel/terminate re-checked at the top of each wait iteration (cancelled task stops
issuing socket work promptly), and the bare `thread::sleep` busy-poll upgraded to a kernel `libc::poll`
fd-wait (early wake on readiness, no wasted syscalls, near-epoll latency).

**`Channel.close()` + closed-channel semantics + `try_send` + `for v in ch:` have landed** (both
engines, branch `feat/channel-close`) — the headline deferred concurrency *feature* (the engine was
already complete through D6b; this is surface breadth). Closes the gap where a consumer looping `recv`
after the producer was done could only **deadlock-fault**; now there is clean producer→consumer
termination. Surface (user-locked):
> - **`for v in ch:`** — blocking iteration over a channel: drains buffered + future values, ends
>   cleanly once **closed-and-drained** (Go's `for v := range ch`). The headline consumer form.
> - **`ch.close()`** — idempotent, no args, returns Nil; wakes **every** parked/demoted receiver.
> - **`ch.send(v)` after close → faults** `"send on a closed channel"`; **`ch.recv()` on a
>   closed-and-empty channel → faults** `"receive on a closed channel"` (drains buffered first).
> - **`ch.try_send(v) -> bool`** — the safe partner of `send` (mirrors `try_recv` vs `recv`):
>   `true` = sent, `false` = closed. Channels are **unbounded**, so closed is `send`'s only failure
>   mode — hence `bool`, not `Option`/`Result`. `try_recv` unchanged (closed is `None`, by design).

Implementation: `ChannelCore` folds a `closed` flag **into the queue mutex** (`Mutex<ChanState{queue,
closed}>`, `src/vm/core.rs`) so "value waiting OR closed" is one atomic observation — killing the
lost-wakeup TOCTOU at `park`/`send_wake`/`recv`/demote. Two new ops (`IsChannel`, `ChanRecvOrClosed`);
`compile_for`'s single-var path became a 3-way runtime branch (channel/struct/seq) where the channel +
struct steps share the existing `Option` `None→exit`/`Some→bind` decoder — the channel step just
produces the `Option` via `ChanRecvOrClosed` (parks on empty-open like `recv`, `None` on
closed-drained) instead of `next()`. `recv` + the op share `chan_recv_step`/`park_recv`;
`demote_recv_block` (in-callback recv) is closed-aware too; `MnSched::close_wake` wakes *all* parked
fibers (vs `send_wake`'s one-per-value) + notifies demoted condvars; `park` re-checks `closed` in its
gap. Interp mirrors it (sequential oracle: `exec_for` channel branch + `eval_channel_method` faults).
Comprehension-over-channel (`[v for v in ch]`) is **rejected by the checker** on both engines (it would
diverge — VM drains, interp oracle can't), steering users to `for v in ch:`. Golden
`examples/parallel_channel_close.chz` (producer-first → parity-safe) is VM-cooperative == interp ==
expected and runs on `--parallel` too. **1455 tests green** (+24: checker close/try_send/for-bind/
comprehension-reject; interp + VM close/send-fault/recv-fault/try_send/drain/double-close/len/try_recv;
`--parallel` close-wakes-one / close-wakes-many / dead-consumer-terminates / send-after-close-faults;
golden ×2), `cargo clippy` clean, conformance green, racy `--parallel` close tests stress-clean (6×
each). **2-agent S++ panel** (concurrency-correctness + API-design): concurrency reviewer found zero
Critical/Important (traced every close/park interleaving safe under 90× stress); API reviewer found one
Important — comprehension-over-channel parity divergence — **applied** (checker rejection + test), plus
the two minor close-then-`len`/`try_recv` contract tests and a softened `send`-atomicity comment.

**D6c — per-socket read/accept/write timeout (`--parallel`) — has landed** (branch
`feat/channel-close`). The deferred user-facing socket timeout the tier-D doc flagged ("the timer fold
is the groundwork"). Surface (return types **unchanged** — a timeout is the existing `Err` variant):
`conn.read(n, timeout_ms)` / `sock.write(s, timeout_ms)` / `server.accept(timeout_ms)` return
`Err("timeout")` when no data / writability / connection arrives within `timeout_ms`; `0` polls once
(never parks), a negative saturates to `0`. `--parallel`-only by construction (the cooperative engine
has no fiber to park and already fails loud on would-block; either way its result is an `Err`).
Mechanism reuses D6b's deadline-bounded poll with **no new thread/heap/job**: `poller::Parked` gains
`deadline: Option<Instant>`, `next_timeout` folds the earliest socket deadline in with the timer heap,
and a new `fire_due_socket_timeouts` pass (after the ready-fd inject loop, same registry lock →
readiness wins ties) sets a per-fiber `poll_timed_out` marker (carried across `swap_ctx` like
`pending_connect`); the rewound socket op's re-run sees the marker at method entry and returns
`Err("timeout")` instead of retrying the syscall. Checker gained optional trailing-arg arity
(`FnSig::min_params` + `check_args_range`). New `examples/socket_timeout.chz`; poller units
(`register_with_deadline_times_out_when_fd_never_ready`, `readiness_before_deadline_wins`,
`deadline_past_fires_immediately`, `socket_timeout_and_timer_share_one_thread`) + e2e
(`read_timeout_returns_err`, `accept_timeout_returns_err`, `read_without_timeout_still_parks_forever`)
+ 7 checker arity tests. In-callback `demote_block_socket` timeout is out of scope v1 (documented).

**Pending-`spawn`-drop on early `parallel:` escape → cancel-and-report — has landed** (both engines,
branch `feat/channel-close`). Closes the structured-concurrency gap (gaps.md): a `parallel:` body that
escaped via `?`/`return`/`break`/`continue` **before the join** mishandled already-`spawn`ed-but-
unstarted tasks — and the engines **diverged** (interp ran them on `return`/`break`, dropped on `?`; VM
dropped on all three). Policy (decided): unstarted tasks are **cancelled, not run** — the same end-
state a started sibling reaches under B3.4 — and one stdout report line (`runtime::pending_cancel_report`,
byte-identical across interp / VM-cooperative / VM-`--parallel`) is emitted when ≥1 task is cancelled;
the escape propagates unchanged, nursery depth returns to 0 (no leak). VM routes a new
`drain_escaped_nursery` through **four** reclaim sites: `do_return`, the recover-catch fault path, a
**net-new `Op::ReclaimNursery`** for break/continue (compiler emits one per escaped nursery scope via
`emit_loop_nursery_drain`, mirroring the defer-scope drain), and the `do_try` recover-scoped-`?`
short-circuit. **2-agent S++ panel** caught one **Critical** on the `do_try` path — a recover-scoped
`?` escaping a `parallel:` whose **body has a `defer`** ordered the cancel-report *before* the body
defer, diverging from the interp (which runs body defers as the `?` unwinds, then reports). **Fixed**:
a per-nursery `nursery_defer_floors` stack (captured at `EnterNursery`, swapped with `nurseries` across
`swap_ctx`) lets the `do_try` path drain the escaped body's defers down to its floor *before* the
report, then the recover-block defers after — interp order restored
(`parallel_recover_scoped_try_orders_report_after_body_defer`, all three engines). Task-A Minor (net.rs
doc on `timeout_ms==0` cooperative behavior) also applied.

**Latest suite: 1475 tests green** (unit + parity + `cargo test conformance` 7/7), `cargo clippy
--all-targets` clean; `primes_parallel=148933` and `examples/parallel*.chz` byte-identical both engines.

**Per-connection `spawn` LANDED — eager injectable nursery (`--parallel` M:N, ≥2 cores).** A `spawn` in
a *nested* `parallel:` body now runs CONCURRENTLY with the rest of the body instead of being queued for
the join, so the canonical server shape works: an accept loop `spawn`s a `handle(conn)` fiber per
connection and keeps accepting while handlers run. **Mechanism:** a nested nursery (entered inside a
live fiber, `mn.is_some()`, on ≥2 hw threads) is now **eager** — `EnterNursery` builds the `MnSched`
immediately (`activate_eager_nursery`, total starts 0, `body_open=true`, spawns ONE dedicated **raw OS
thread** as the body's drainer), a `spawn` **injects** a live `Pending` fiber straight into that sched
(`MnSched::inject` assigns the slot index under the lock, grows `total`+`slots`, queues runnable,
notifies — the `complete_offload` twin), and `JoinNursery` `close_body`s + runs the inline join worker
to drain remaining handlers + join the drainer + reduce (Decision-F flush in spawn order). The new
`body_open` flag holds `finish`/`take_runnable` termination open and vetoes `is_deadlocked` while the
body may still inject (top-level/lazy nurseries set it `false` → D2b path byte-identical). The open
scope lives on a per-fiber `FiberCtx::eager_scheds` stack (lockstep with `nurseries`; swapped across a
park; reclaimed by `drain_escaped_nursery`/recover-catch). A handler fault trips the inner cancel
(D6b `cancel_drain`+`drain_sched`), surfaces as the acceptor's fault at the join, and the outer nursery
then cancels the clients — no hang. **Why a raw thread (not the bounded pool):** the eager body has no
inline worker until the join, so liveness during the body depends on the drainer — a pool helper is the
wrong tool (a 1-core box farms zero helpers; nested eager nurseries would exhaust the fixed pool, an
undetectable hang since `body_open` vetoes the deadlock predicate). One raw thread per open eager
nursery is unconditional + pool-independent (verified: 4 concurrent eager servers complete; the old
pool-farmed design hung). **v1 limits (documented):** (1) **needs ≥2 hw threads** — an eager inner join
blocks the parent's OUTER worker (decision B); a handler servicing an outer-sibling client needs that
sibling to run, impossible if the outer nursery is single-worker (1 core). On 1 core we fall back to
the lazy queue-at-join path (which itself can't service a nested socket server — a pre-existing M:N
limit; `--parallel` on 1 core is already degenerate). (2) bounded accept loops only (an unbounded
`while true:` server never reaches the join → the scope never completes; graceful shutdown is future
work). (3) a handler talking BACK to the acceptor via a Channel is a cross-nursery wakeup (handlers
reach clients via sockets, OS-mediated, which works). **+8 tests** (2 `MnSched::inject` units, 1
`eager_scheds` swap unit, `net_echo_server_spawns_handler_per_connection`,
`net_echo_sequential_client_needs_concurrent_handlers`, `net_echo_handler_fault_cancels_acceptor`,
`eager_nursery_with_zero_spawns_completes`, `net_concurrent_eager_servers_do_not_exhaust_pool`; the
three socket e2e tests skip on 1 core); new `examples/echo_server_spawn.chz` serves 50 conns
one-fiber-each. Found + fixed via a 2-agent S++ panel: the bounded-pool-farming design hung on 1 core
and under nesting (replaced with the raw drainer). **1483 tests green**, `cargo clippy` clean.

> **DECISION — do NOT build interp B1/B2 (suspendable tree-walker). This is a deliberate non-goal,
> not a TODO.** The interpreter stays frozen at the **sequential concurrency subset** and serves as
> the **differential-testing parity oracle** for the non-blocking language surface (its real value:
> catching VM / GC / compiler bugs). Giving it suspendable execution would need stackful coroutines
> or a full CPS rewrite of `eval` — a large, risky cost to cover a narrow slice the oracle does not
> need. **The VM is the sole concurrent engine.** Future sessions: spend effort on B3/B4/B5, not on
> closing this gap. Revisit only if interp maintenance ever costs more than the bugs it catches.

**Parity contract (narrowed, intentional):** the engines agree on the **sequential subset** —
including all *non-blocking* `parallel:` / `spawn` / `Channel` / `Shared` / `Executor` programs
(C1–C5 goldens, byte-identical, parity-tested). **Suspendable concurrency (blocking `recv`) is
VM-only by design**: under `--interp` a blocking `recv` faults `deadlock`, pinned by
`interp::tests::channel_block_chz_faults_deadlock_on_interp` vs the VM golden
`golden_channel_block_chz_matches_expected`. This divergence is the stated contract, not a bug.

**Known VM v1 limits (acceptable; not parity issues):** a blocking `recv` cannot suspend inside a
native callback (list HOFs, `sort`, `compare`/`hash`/`str` hooks, `Shared.update`, the executor
drain, or a `defer`red call) — it faults `deadlock` (the callback's loop/recursion state lives on the
host stack, not in a fiber); and a fiber in an outer nursery cannot be woken by progress in an inner
one (structured-concurrency scoping).

**Group A status (sequential refinements, no engine rewrite):** **A2 (`Executor` program-exit
auto-drain) is done** (this session, both engines). **A3a** (reject a non-sendable read smuggled
through a *nested closure* in a `spawn:` block) was found **already enforced** — emergent from the
persistent `capture_floors` + the `infer_ident` read gate — and is now **pinned by a regression
test**. **A1** (`Channel.try_recv`) — originally dropped (its mid-flight-producer scenario needed the
engine), **now shipped on both engines** once B1/B2 unblocked it (a non-blocking poll runs identically
on the interp, so it stays parity-tested). **Still dropped:** **A3b** (`Executor.submit` capture gate)
— `submit` runs the closure in-heap at the drain, so gating it now would wrongly reject valid programs
(lands with Group B).

**Permanent non-goals:** **interp B1/B2 (suspendable tree-walker)** — see the DECISION box above;
`yield`/generators, variadic args, Level-3 dynamic `cdylib`/C-ABI FFI, bignum (`i64`-only — every
overflow is a recoverable fault; binary work → a future `bytes` *sequence*, no `byte`/`u8` scalar).

---

## Done (newest → oldest)

Each landed TDD, both engines in lockstep, with a golden + parity `examples/*.chz`. Git has the detail.

- ✅ **Concurrency D4a–D4d — Go-style work-stealing per-worker run queues** (`--parallel` engine;
  cooperative untouched, byte-identical). Replaced D2b's single shared run queue with a **per-worker
  `LocalQ`** (`runnext` + ring, lock class **B**) + a shared `global` overflow queue (`SchedCore`,
  lock class **A**). `take_runnable(wid, tick)` order: periodic global pull every `GLOBAL_CHECK_INTERVAL`
  (61) schedules → own local → **work-steal** (`try_steal`: rotating victim, ceil-half from the ring
  back, falling back to the victim's `runnext`) → **capped global batch-grab** (`globrunqget`:
  `min(g/nworkers+1, g, LOCAL_RING_CAP/2)` into the own local — one core-lock acquisition amortized
  over the batch is the contention win) → park. **D4a** introduced `runnable: AtomicUsize` (count of
  fibers queued in any local + `global`) and rewired the deadlock predicate to
  `running==0 && runnable==0 && parked>0 && done<total` (no single queue to `.len()` under the split;
  byte-identical `DEADLOCK_MSG`). **D4b** split the queue + threaded `wid` (scaffold, locals unused →
  behavior identical). **D4c** added stealing + the batch-grab + `cv.wait_timeout(2ms)` **bounded-poll
  wake** in place of the full Go `wakep` StoreLoad barrier — a correct, simpler intermediate: once a
  fiber can land in a local outside the core lock a plain `notify_all` is lost-wakeup-prone, so the
  timeout caps that to ≤2ms latency, **never a hang** (mirrors B3.4's 50ms recv-cancel re-check);
  liveness still rests on the always-running inline shell (decision B), which `try_steal` lets reach
  any local. **D4d** added the periodic global check. **Key design call:** a time-slice `yield` goes
  to **global** (Go-faithful fairness — routing it to the worker's own local would let a CPU hog
  re-pop itself forever, re-introducing the D3 starvation), and `send_wake`/`park`-requeue/
  `cancel_drain` stay on global too; **only the batch-grab populates locals**, fed from global and
  rebalanced by stealing. **Per-queue `Mutex`, not lock-free CAS** (a `Fiber` is a large move-only
  struct). Lock order strictly **B-then-A / A-then-C** → no ABBA. TDD: 8 new tests
  (`runnable`-tracking, `LocalQ` ordering, local-before-global, steal-half, steal-skips-self,
  periodic-61, + the `d4_worksteal_cpu_and_channel_stress` watchdog — 500 consumers + 500 CPU
  producers exercising grab/steal/yield/park/wake/`wait_timeout` together). **1372 tests** green,
  `cargo clippy -- -D warnings` clean, `cargo test conformance` clean, `primes_parallel=148933` both
  engines, all `--parallel` goldens byte-identical; full suite ×5 + stress ×15 + defer/cancel race
  ×40 stable. 2-agent S++ concurrency panel (SRE + invariant/VM): zero Critical; both Importants
  applied — a `notify_all` after the batch-grab surplus push (kills a 2ms steal-latency cliff on
  quiet-after-fan-out workloads) and `try_steal` now drains `runnext` (forward-safe: keeps the
  deadlock predicate sound if a future commit ever routes work through `runnext`). **D4e — the full
  SeqCst `wakep` StoreLoad barrier + spinning-worker that removes the poll — is the remaining D4 owe**
  (correctness does not depend on it; it is a throughput refinement).

- ✅ **Concurrency D3 — reduction-counting preemption (BEAM-style fairness)** (`--parallel` engine).
  Before: an M:N fiber held its worker until it parked on `recv` or finished, so a CPU-bound fiber
  with `#runnable ≫ #workers` starved every sibling queued behind it. Now: a fiber carries a
  reduction budget `reds: u32` (reset to `CONTEXT_REDS = 4000` on every schedule-in in
  `run_one_fiber`); the existing `run_until` loop-top safepoint — beside the GC + cancel checks —
  decrements it **per dispatched op** under the M:N engine (`self.mn.is_some()`) and, at exhaustion
  with `native_reentry == 0`, sets `yield_now` and returns `Ok(())` to stop dispatch (the same
  `native_reentry` guard as `recv`-park; a yield inside a native callback is deferred until the
  reentry unwinds). `run_one_fiber` maps that to a new `Disp::Yield`; `mn_worker_loop` calls
  `MnSched::yield_fiber`, which under the sched core lock does `running--` + `runq.push_back` +
  `notify_all` — requeue at the **tail** for round-robin, no `parked` bucket touched (so no park-gap
  re-check, and `take_runnable` pops `runq` before the deadlock predicate → no false deadlock). The
  yield reuses the recv-park suspend/rewind contract (frames stay live, resume re-enters
  `run_until(0)` from the saved `ip`), so it must unwind **every nested `run_until` level** without
  popping a result: a `paused()` helper (`suspend.is_some() || yield_now`) replaced `suspend.is_some()`
  at each propagate-up gate (`run_proto`, `do_call`, `do_method_call`, `run_until` bottom,
  `start_task`). That fix closed a found bug — a yield deep in a call chain
  (`main→worker→count_primes→is_prime`) let `run_proto` pop a live operand-stack temp as a bogus
  return value → `expected bool, found int` on `primes_parallel`. Cooperative engine byte-identical
  by construction (`yield_now` gated on `mn.is_some()`, always `None` there). TDD: a fairness
  hang-watchdog (64 spinning CPU hogs ≫ pool + 50 short fibers — hangs without preemption, the
  watchdog turns the hang into a test failure + standing regression guard), a 10 k-fiber soundness
  churn, the nested-call unwind regression, and a `MnSched::yield_fiber` unit test. **1365 tests**
  green, `cargo clippy` clean, `primes_parallel=148933` both engines, all `--parallel` goldens
  byte-identical; 4-agent S++ backend review panel (Godot Gameplay / Solidity / Incident Response /
  SRE), zero real findings.

- ✅ **Concurrency D2b — M:N scheduler: park-on-`recv`, not thread-per-task** (`--parallel` engine).
  Old: one full worker `Vm` per task on a bounded FIFO pool; an empty `recv` **blocked the whole OS
  thread** on a condvar, so `#blocked-tasks > #pool-threads` starved/hung. Now: tasks are lightweight
  **fibers** (each owns its `Heap` + per-task `out`/`stderr`/`module_objs`/`module_faulted`/`executors`,
  all carried in `FiberCtx` and swapped via `swap_ctx` — the D2a foundation) multiplexed over the pool.
  A new `MnSched` (one `Mutex<SchedCore>` + `Condvar`) holds a shared run queue + a per-`ChannelCore`
  park set + task-order outcome slots. `mn_worker_loop` (the cross-thread generalization of the
  cooperative `run_child`): pop a fiber → `swap_ctx` in → `start_task`/`run_until(0)` → on empty
  `recv` PARK it (reuse the cooperative suspend/rewind-`ip` mechanism, file into the channel's wait
  set) and grab the next; `send` (`MnSched::send_wake`) enqueues the message **and** re-queues parked
  waiters **atomically under the sched lock** (core-OUTER / channel-`q`-INNER everywhere → no ABBA);
  `park` re-checks the queue **and** cancel flag under that same lock to close the check-then-park
  lost-wakeup gap. Deadlock is the exact predicate `running==0 && runq empty && parked>0 && done<total`
  (no barrier-confirm epoch dance — a single coordinator has global knowledge), reusing `DEADLOCK_MSG`.
  Decision F flush + `Exit`-over-`Fault` precedence factored into `reduce_task_slots` (shared with the
  legacy executor-drain path). The joining thread runs an **inline shell that alone drains the whole
  run queue** (decision B), so liveness never depends on a bounded pool resource — farmed helper shells
  are fire-and-forget, never joined, which kills the nested/concurrent pool-exhaustion join hang. The
  legacy condvar-`recv` branch + `DeadlockWatch`/`WatchState`/`task_finished` were retired. Headline:
  1000 consumers + 1000 producers on the core-sized pool finish in ~0.02 s (would starve on the old
  engine). **1361 tests** (incl. `mnsched_*` mechanics + park-gap regressions, `mn_many_blocked_consumers_complete_without_starving`,
  `mn_thousand_fiber_pipeline_completes`); 5× full-suite + 60× the defer/cancel race + 10× headline,
  all green; `cargo clippy` clean; `primes_parallel=148933` both engines; all `--parallel` goldens
  byte-identical. 4-agent S++ review panel + cold pass: **two Criticals found and fixed** — (1) a
  `parallel_defer_runs_on_cancelled_sibling` race (a sibling fault could trip cancel before the
  consumer registered its `defer`; fixed by synchronizing the test with a start-token, matching the
  Go semantic that an unregistered defer doesn't run), (2) the nested/concurrent pool-exhaustion join
  hang (fixed by the fire-and-forget farm + inline-shell liveness above). Per-worker local rings,
  work-stealing, the targeted-wake StoreLoad barrier, and cross-nursery wakeups remain D4+ (decision D
  cross-nursery/`Executor` hangs documented). Subsumes D1's deferred heap-into-`FiberCtx` half (D2a).

- ✅ **Concurrency D1 (lazy module snapshot) — kill the per-task module-graph rebuild** (`--parallel`
  engine). Old: `prepare_worker` / `prepare_worker_from_wire` called `build_worker_modules`, which
  **eagerly reconstructed the entire parent module graph into every worker heap, per task** (N tasks
  → N full rebuilds via `map_global_value`). Now: `snapshot_modules` builds a heap-independent,
  read-only `Arc<ModuleSnapshot>` **once** (memoized on the top-level VM in `snapshot_memo`; a nested
  worker reuses its installed `module_snapshot` Arc since `--parallel` globals are frozen, G1), shared
  by every worker via a cheap `Arc` clone. Each worker pre-allocs **empty** module objs
  (`install_snapshot`, index order preserved so home indices line up) and **faults a module's globals
  into its heap lazily on first access** (`fault_module` / `replay_snap`, gated by
  `module_faulted: Vec<bool>`) — a task that touches only its home module rebuilds only that module,
  one that touches none rebuilds nothing. `SnapValue` mirrors the deleted `map_global_value`
  structural recursion exactly (Func/Closure home → `module_objs` index, import-alias → `ModuleAlias`,
  `Native` fn-pointer, containers element-wise, value-derived map/set hashes carried). Lazy fault-in
  is hooked at the four module-global read sites — `Op::GetGlobal`, the `Op::GetCaptured` home
  fallback, `get_field` (module member), and the `module.fn(...)` dispatch — each preceded by
  `ensure_module_faulted` (a no-op on the top-level / cooperative VM, which never fault: their
  `module_objs` are the real populated modules, `module_snapshot` stays `None`). **The
  `Heap`-into-`FiberCtx` half of the literal §D1 spec is deferred to D2**, where the M:N share-nothing
  fiber model makes it observable; under the unchanged FIFO pool it buys nothing and would risk the
  cooperative share-by-ref single heap (decision A). 2 new characterization units (sibling-fn + global
  resolution under `--parallel`; 2 000-spawn correctness + loose wall-clock ceiling); all `worker_*`
  reconstruction units + `--parallel` goldens byte-identical; `primes_parallel` still prints
  `148933` on both engines. Two parallel review-panel reviewers returned clean (no Critical/Important);
  applied the comment-only `module_global` invariant note for future read sites. **1346 tests** green,
  clippy clean.
- ✅ **Concurrency D0 — O(N²)→O(N·logN) cooperative ready-queue** (VM cooperative engine only;
  `--parallel` unaffected). `run_scheduler` no longer linear-scans every live child per turn
  (`pick_runnable`, deleted); each `Nursery` carries a `ready: BTreeSet<usize>` (lowest-index pop —
  byte-identical scheduling order to the old scan) + a `blocked_on: HashMap<usize, Vec<usize>>` of
  parked indices. A `recv`-park registers its index; a sibling `send` (`wake_on_send`) drains the
  bucket back onto `ready`. 50k trivial fibers: ~7 s (debug, old) → tens of ms.
  **Three deliberate deviations from the literal `docs/concurrency-tier-d.md` §D0 spec, each verified
  against the code:** (1) **key `blocked_on` by `ChannelCore` pointer, not `GcRef`** — cooperative
  `spawn` deep-clones a channel (`from_wire` allocs a fresh handle onto the same `Arc<ChannelCore>`),
  so siblings hold distinct handles aliasing one core; a handle key would lose every wakeup. (2)
  **`BTreeSet` (lowest-index-first), not `VecDeque` (FIFO)** — FIFO would re-queue a woken low-index
  fiber behind pending higher-index ones, reordering output; the `BTreeSet` reproduces the old
  scan's order exactly (goldens byte-identical). (3) **`wake_on_send` drains every scheduler level,
  not just the innermost** — preserves the old re-scan's cross-level wakeup (an inner-nursery `send`
  waking an outer parked sibling). `FiberState::Blocked` dropped its now-redundant `GcRef` payload
  (the receiver handle stays GC-rooted on the fiber's operand stack). 3 new fiber units (50k-scale
  ceiling, many-producers/one-consumer over the core-ptr map, cross-level wakeup); review-panel
  finding applied (the core-ptr resolver fails loud via `unreachable!`, matching `channel_core`,
  rather than a silent sentinel key). **1344 tests** green, clippy clean.
- ✅ **Concurrency B3.3c/d — worker module-graph reconstruction** (VM, single-thread, parity-preserved).
  `Vm::build_worker_modules` + `map_global_value` snapshot the parent's initialized module graph into
  the worker heap (read-only `home`): tasks read post-init globals + call sibling/imported fns; method
  tasks (`spawn obj.m()`) dispatch via the rebuilt `module_objs`. Structural container recursion keeps a
  nested callable from smuggling a parent `GcRef` across the airlock. `run_task_isolated` is now
  functionally complete bar real threads (still test-only until `--parallel`). 7 new `worker_*` units
  incl. a GcRef-smuggle regression + a `gc_stress` reconstruction test. `docs/concurrency-b3.md`
  B3.3c/d rows + landed note. **1312 tests** green, clippy clean.
- ✅ **Concurrency B3.3b — G1 module-globals checker gate** (checker, parity-preserved). A
  reassignment (`=`/`+=`/`-=`) of a module global reachable — directly or transitively through
  free-function calls — from a `spawn` task is a type error (*"…use Shared[T]"*). New
  `Checker::check_spawn_global_mutation` + scope-aware AST walkers (`collect_spawn_roots` /
  `collect_free_calls_*` / `find_global_mutations` / `find_mutations_in_expr`): flow-scoped to spawn
  reachability, transitive over the free-fn call graph (cycle-guarded), and scope-aware down to
  closure-params/comprehension-vars so a shadowing binder is never mis-flagged; descends `recover:`
  blocks. Direct in-`spawn:`-block writes stay caught by the existing `is_captured` gate. 4-agent S++
  panel + cold pass caught a shadowed-spawn-target false positive and a `recover:` false negative
  pre-merge. Documented gaps (→ B3.3-threads): global-closure spawn targets, method chains. 16 new
  checker tests. `docs/concurrency-b3.md` B3.3b row.
- ✅ **Concurrency B3.3a — `str` crosses the airlock by value** (VM, single-thread, parity-preserved).
  Owned-bytes `WireValue::Str(Box<str>)` arm; `to_wire`/`from_wire`/`display_wire`/`collect_core_gcrefs`
  handle it; `str` is no longer a by-reference `Handle`, so `ensure_crossable` lets `str` (and data
  containing it) cross a worker boundary. Parity-safe (immutable, value-compared, no identity operator
  → fresh handle is unobservable; cached map/set hashes preserved). 3 new VM units (incl. str map-key
  round-trip); the B3.2 str-rejection test became `worker_crosses_str_by_value`. All concurrency +
  GC-stress goldens byte-identical. `docs/concurrency-b3.md` B3.3a row.
- ✅ **Concurrency B3.2 — `Arc<Program>` + isolated worker-VM construction** (VM, single-thread,
  parity-preserved). `program: Rc<Program>` → `Arc<Program>` (the compiled program is immutable after
  compile, so a worker shares it read-only — `Program` is plain owned data, `Send + Sync`). New
  `Vm::spawn_worker` builds a fresh worker `Vm` with its **own heap** sharing `Arc::clone(program)`;
  `Vm::run_task_isolated` lowers a `spawn`'d function/closure task to its `ProtoId` + wire'd
  captures/args (the callee is **never** crossed as a parent-heap `GcRef` — the proto lives in the
  shared `Arc<Program>`), `from_wire`s them into the worker heap, rebuilds the closure over a fresh
  empty `home`, runs it **synchronously** (no threads), and crosses the result + per-worker
  `out`/`stderr` back as a `WorkerResult` (decision F). **Cross-heap safety enforced** —
  `WireValue::has_handle` + `Vm::ensure_crossable` reject any `str`/closure value (a dangling `GcRef`
  in another heap) on captures, args, **and the returned result** with a clean fault instead of silent
  corruption; method tasks gated off (worker `module_objs` is empty). All `#[allow(dead_code)]` until
  B3.3's `--parallel` wires it in (decision A keeps the cooperative engine the default through B3.2).
  5 new units (distinct-heap / result+out / program-Arc-sharing / str-rejection / method-rejection);
  **1292 tests** green, `cargo test conformance` + `cargo clippy -- -D warnings` clean; all existing
  concurrency goldens + GC-stress byte-identical. Reviewed by 2 parallel S++ panels — the silent-
  dangling-handle risk they flagged is now the enforced `ensure_crossable` guard. `docs/concurrency-b3.md`
  §4 + B3.2 landed note.
- ✅ **Concurrency B3.0 — `WireValue` airlock** (VM, single-thread, parity-preserved). The task-airlock
  deep-copy `deep_clone` (`spawn` / `Channel.send` / `Shared` get-set) is now a **`WireValue`
  round-trip**: `Vm::to_wire` serializes a heap `Value` into an owned, `Send`-shaped `WireValue`
  (`src/vm/wire.rs`) and `Vm::from_wire` reconstructs it into the destination heap — **byte-identical**
  to the old direct deep-copy. Data (list/tuple/map/set/struct/enum) recurses; by-reference objects
  (`Str`, callables, modules, `Channel`/`Shared`/`Executor`) cross as `WireValue::Handle` (the same
  `GcRef`, same heap in B3.0); `Map`/`Set` carry their cached hashes so reconstruction never re-hashes
  (identical order + index). This de-risks the serialization layer before any thread is spawned: B3.1
  swaps the shared-core handle arms for `Arc<…Core>`, B3.3 makes `WireValue` the form that crosses a
  real OS thread. `to_wire` is total in B3.0 (statically infallible — the `Result` + `deep_clone`'s
  `.expect` are forward-plumbing; B3.3 *adds* the real `Err` arms for `Module`/`Func` that can't cross
  a thread). `from_wire` builds bottom-up and `Heap::alloc` never collects, so it inherits
  `deep_clone`'s GC-safety. 3 `wire_*` unit tests (round-trip value-equality over a nested mix; map
  hash/order preservation under a collision; by-handle identity for `Channel`/`Shared`/`Executor`/
  `Str`); all existing concurrency goldens + GC-stress stayed byte-identical green. Reviewed by 3
  parallel S++ reviewers — no correctness/byte-identity/GC findings; the one unanimous note (docs
  claimed a defensive fault arm that doesn't exist yet) was applied (comment-only). Surface unchanged.
- ✅ **Concurrency B3 — decomposition + documentation** (planning session, no engine code). Broke the
  Tier-C OS-thread multicore epic (B3, with B4/B5 folded in) into seven independently-shippable,
  TDD'd phases **B3.0…B3.6** in **[`docs/concurrency-b3.md`](docs/concurrency-b3.md)** — a persistent
  multi-session plan with the validated shared-nothing architecture (per-thread `Vm`+heap;
  `Arc<Program>`; a `WireValue` airlock replacing `deep_clone`; `Channel`/`Shared` cores as
  `Arc<…Core>` outside every heap; bounded pool; cooperative cancel), recorded decisions **A–G**
  (chief among them **A**: keep cooperative single-thread as the *default* and gate OS-thread
  multicore behind `--parallel`, so existing byte-identical goldens + VM==interp parity survive
  untouched), a risk register (top risk: **mutable module globals can't cross threads**), and a
  per-phase TDD focus. B3.0–B3.2 ship behind unchanged behavior; `--parallel` lands at B3.3.
  Also documented the non-B3–B5 backlog (cross-nursery wakeups, recv-in-native-callback,
  `Channel.close()`, A3b) in **[`docs/concurrency.md` §11](docs/concurrency.md)**. Docs-only — no
  `src/` changes; suite unchanged.
- ✅ **Concurrency A1 — `Channel.try_recv() -> T?`** (both engines). A **non-blocking** poll: `Some(v)`
  if the mailbox has a queued value, `None` if empty — it never blocks, faults, or suspends a fiber
  (the opposite of `recv`, which faults `deadlock` / parks under the scheduler on an empty channel).
  One mirrored arm per engine (checker `channel_method_sig` → `Ty::option(elem)`; interp
  `eval_channel_method`; VM `channel_method` via `alloc_enum` — and crucially the VM arm never touches
  `scheduler_stack`/`native_reentry`/`suspend`/`ip`, so it can't route through the `recv` park path).
  Originally **dropped** (its motivating mid-flight-producer scenario was unreachable under
  run-to-completion) and **un-deferred** once B1/B2 landed; because it's non-blocking it runs
  *identically* on the sequential interp and the VM, so it ships on both and stays parity-tested.
  `examples/try_recv.chz` golden (VM + interp byte-identical + GC-stress) + checker type/arity tests +
  per-engine empty/`Some`/in-`parallel`-no-suspend tests + a VM `try_recv`-drains-residue-after-a-
  blocking-`recv`-resumes test (pins the resume path leaves `suspend`/`ip` clean). Reviewed by two
  parallel S++ reviewers — no correctness findings. `docs/concurrency.md` §5/§9 + `docs/syntax.md`.
- ✅ **Concurrency C5 / Group B — B1 + B2 cooperative fibers + blocking `recv`** (VM engine). The
  bytecode VM gained *suspendable execution*: a `recv` on an empty channel under an active `parallel:`
  scheduler **parks** the running fiber (rewind-and-retry at the instruction boundary — push the
  receiver back, `ip -= 1`, set a suspend flag that breaks `run_until`/the re-entrant call path
  without unwinding defers) and the **nursery-local cooperative scheduler** runs a runnable sibling,
  resuming the parked fiber once its channel has data. A child that never blocks still runs to
  completion FIFO, so non-blocking programs are byte-for-byte unchanged. Each fiber owns its full
  execution context (`frames`/`stack`/`call_depth`/`cur_base`/`handlers`/`nurseries`/`fault_trace`),
  swapped in/out around scheduling; parked fibers are GC-rooted; nested `parallel:` recurses into a
  fresh scheduler level; a child fault or `std.os.exit` aborts its siblings. A wide native-reentry
  guard converts a `recv` that can't be parked (inside a HOF/sort/`compare`/`hash`/`str`/`update`/
  executor-drain/`defer`) into the deadlock fault. `examples/channel_block.chz` golden (VM + GC-stress)
  + ping-pong, deadlock-detection, guard, nested-`parallel:`, recover-in-child, and os.exit-in-child
  tests. **VM-only** — interp parity is a later milestone (gap pinned by an interp test). See the
  Current-focus parity-gap note and [`docs/concurrency.md`](docs/concurrency.md) §9.
- ✅ **Concurrency C5 / A2 — `Executor` program-exit auto-drain** (both engines). An executor
  submitted to but never explicitly `shutdown`/`shutdown_now`-ed is now gracefully drained at a clean
  program exit (FIFO, creation order) instead of silently dropping its queued work — mirrors a
  top-level `defer ex.shutdown()`. A per-engine **executor registry** (interp `Vec<Rc<RefCell<…>>>`;
  VM `Vec<GcRef>` that also joins the GC root set so un-shut work survives to the drain) drives it via
  the shipped `shutdown` path (first-fault-aborts-siblings). Hooked into every driver
  (`run_program_inner` / `run_with` stress / `run_file_inner`, both engines). A hard `std.os.exit`
  skips it (like `defer`); a faulting program is not drained. Also pinned **A3a** with a regression
  test — a non-sendable read smuggled through a *nested closure* in a `spawn:` block is rejected
  (already enforced, emergent from `capture_floors`). `examples/executor_autodrain.chz` golden + VM/
  interp parity + GC-stress + os.exit-suppression + fault-propagation tests. *Dropped: A1, A3b
  (see Current focus).*
- ✅ **Concurrency C5 (sequential subset) — `Executor` escape hatch** (both engines). `Executor()` +
  `submit(fn())` / `shutdown()` / `shutdown_now()`, reaped via `defer ex.shutdown()` (docs
  [`concurrency.md`](docs/concurrency.md) §8). New `Ty::Executor` (non-generic, sendable handle,
  reserved type name); interp `Value::Executor(Rc<RefCell<ExecState>>)`; VM `Obj::Executor { queue,
  shut }` + `Op::NewExecutor` + GC child-tracing. `submit` enqueues by handle (rejected once shut);
  `shutdown` drains the **live** queue FIFO one task at a time via the re-entrant call path (first
  fault aborts the rest + propagates, like a nursery; not-yet-run siblings stay for a later reap);
  `shutdown_now` discards pending. Both engines drain the live queue identically (a re-entrant
  `shutdown_now`/fault mid-drain behaves the same) — parity-pinned. `examples/executor.chz` golden +
  VM/interp parity + GC-stress + re-entrancy/fault-during-drain tests. *Deferred to real-C5:*
  program-exit auto-drain + closure-capture sendability gating (see Current focus).
- ✅ **Concurrency C5 refinement — `spawn:` block read sendability gate** (checker). A non-sendable
  *function-local* capture merely **read** inside a `spawn:` block (e.g. capturing a closure and
  calling it) is now a compile error, not just a *reassignment* (closes the C2-era gap). Module
  imports / top-level bindings are excluded (globals resolvable in every task, like free functions),
  so reading an imported module inside a task stays legal.
- ✅ **Concurrency C5 refinement — `StructInfo` origin flag** (checker). The `Ref[T]` non-sendability
  gate now keys on a `StructOrigin::{Builtin,User}` flag (threaded from `check_graph` via a
  `current_module_is_stdlib` flag set per module) instead of a bare struct-name string — so a *user*
  struct merely named `Ref` is sendable, while the builtin `std.ref` `Ref[T]` stays non-sendable.
- ✅ **Concurrency C4 — VM parity for `spawn`/`parallel:`/`Channel`/`Shared`** (bytecode VM +
  compiler). Ported C1–C3 off `--interp`-only onto the default engine: heap `Obj::Channel(VecDeque)`
  / `Obj::Shared(Value)` with GC child-tracing; ops `EnterNursery`/`JoinNursery`/`SpawnCall`/
  `SpawnMethod`/`SpawnBlock`/`NewChannel`/`NewShared`; a VM `deep_clone` (data deep-copied, str/func/
  closure/module/Channel/Shared by handle — mirrors interp). The `spawn:` block compiles to a
  synthetic zero-arg closure proto captured like any closure. Sequential executor: a `nurseries`
  stack drains FIFO at the join, first error aborts siblings; pending tasks are GC roots; a
  `recover:` boundary reclaims a fault-orphaned nursery via `Handler::nursery_len`; `Shared.update`
  re-roots the box across its re-entrant call. Differential parity goldens for all three examples +
  micro-tests + GC-stress tests. The four staging-error stubs are gone. *No checker changes* (it was
  already engine-agnostic). Reviewed by two parallel S++ reviewers — no Critical/Important findings.
- ✅ **Concurrency C3 — `Shared[T]` cross-task mutable box** (interp). `Shared(v)` (value-first — the
  element type is inferred from `v`, unlike `Channel[T]()`); methods `get()->T` (copies out), `set(T)`
  (copies in), `update(fn(T)->T)` (read-modify-write; releases the box borrow before calling the user
  fn so a re-entrant `get`/`set` can't panic). The handle is sendable and copied across the airlock —
  every task reaches the one box, whose single owner serialises writes (no locking under the sequential
  executor). The element type is *not* sendability-gated (only the handle crosses — the surprising
  asymmetry vs `Channel`, locked by a test). `Ref[T]` (the in-task box, `std/ref.chz`) is now forced
  **non-sendable** so passing it across a `spawn` is a compile error pointing at `Shared` (spec §7).
  *Known limit:* the `Ref` gate is a struct-name check (a user struct named `Ref` would also be
  non-sendable) — a `StructInfo` origin flag is the principled fix, deferred. `examples/shared.chz`.
- ✅ **Concurrency C2 — `Channel[T]` + sendability** (interp). `Channel[T]()` buffered/unbounded
  FIFO mailbox; methods `send` (move-on-send, deep-copied across the airlock), `recv` (FIFO; empty =
  deadlock-detect fault, not a hang), `len`. A `sendable(Ty)` predicate gates channel element types,
  `spawn` arguments, and `spawn:` capture reassignment — recursing into struct/enum fields (a closure
  smuggled inside a struct field is caught) with a cycle guard. `spawn`'s call target is restricted to
  a function/method like `defer`. `examples/channel.chz` (the canonical fan-out worker).
- ✅ **Concurrency C1 — `spawn` / `parallel:` nursery** (interp, sequential executor). `parallel:` is a
  structured-concurrency nursery; `spawn f(x)` (form 1) and `spawn:` block (form 2) register tasks that
  run to completion FIFO at the dedent (first error aborts siblings + propagates, composing with
  `recover:`/`defer`). `spawn` legal only inside a `parallel:` (checker `nursery_depth`, reset across fn
  boundaries). `deep_clone` isolates task data across the airlock; channels/functions pass by handle.
  Grammar + conformance updated. `examples/parallel.chz`.
- ✅ **Integer overflow policy** — every `i64` overflow is a recoverable fault (never wrap/crash);
  closed the last leak (`std.math.abs(i64::MIN)` → `checked_abs`). `examples/overflow.chz`.
- ✅ **Gaps pass II** — `Ref[T]` mutable box (pure-Chezzi `std/ref.chz`); `sort_by_key`; call fn-typed
  field `self.f(x)`; relax non-const defaults (no param/field refs); runtime stack traces (error line
  + call chain, identical on both engines).
- ✅ **Scripting-ergonomics gap pass** — hex/bin/oct literals; list `.concat`/`.extend` + map
  `.merge`/`.update`; tuple-destructuring `for` + `std/iter.chz` `enumerate`/`zip`; optional chaining
  `?.` + null-coalescing `??`; general tuple destructuring + match-on-tuple + guards.
- ✅ **Fix — loop variable is immutable** — checker rejects assignment to a `for`-loop var (was a
  VM/interp divergence); inner `:=` shadow stays mutable.
- ✅ **M18 — `defer` → block/lexical scope** — runs when its enclosing block exits on every path
  (fall-through / break / continue / return / `?` / panic), LIFO, inner-block-first. Supersedes M17.
- ✅ **M17 — `defer` (Go-style, frame-scoped)** — runs at frame exit, LIFO; receiver+args evaluated
  at the `defer` statement.
- ✅ **M16 — comprehensions + `std.os.exit(code)`** — `[e for x in it if g]` (+ set/map forms),
  first-class AST node; hard uncatchable cooperative exit threaded through both run drivers + CLI.
- ✅ **M15 — slicing + `Index`/`IndexSet`/`Slice` protocols** — `xs[1..3]` half-open/clamped;
  list/map/str conform intrinsically, user structs structurally.
- ✅ **M14 — method-level type params** · **user-defined parameterized protocols** (concrete-arg
  bounds, generalizing `Iterator[T]`) · **default + named args on methods** (desugar-pass).
- ✅ **Default + named arguments** — free fns + struct ctors; scope-aware desugar pass, both engines
  consume an already-normalized AST.
- ✅ **Tech-debt sweep** — reject dup generic param `[T, T]`; nested `set` equality parity; explicit
  call-site type args `name[T,…](…)`.
- ✅ **M11 — panic recovery + Go-style errors** — 2-param `Result[T, E]` (`T!`/`T!E`), `Error`
  protocol (`str` conforms), `recover:` boundary catching any transitive runtime fault.
- ✅ **M10 — type-system depth** — `Stringable`, `Hashable`, per-operator `Add`/`Sub`/`Mul` protocols,
  multi-bound `T: A + B`, transparent type aliases, generic enums; `map`/`set` reworked into real
  insertion-ordered hash tables (any `Hashable` key/element).
- ✅ **M9 — Tier-2 stdlib** — `std.regex` (the `regex` crate) + `std.request` (`ureq`+rustls, blocking).
  First runtime deps; language stays single-threaded/sync.
- ✅ **M8 — Tier-1 stdlib** — `s.chars()` + iterable strings; `std.json` (pure-Chezzi parse/stringify
  + type-directed `decode[T]`); native `std.process`/`std.fs`/`std.time`; `set` type.
- ✅ **M7 — generics + structural protocols** — type-erased generic fns/structs, Go-style `protocol`s,
  `Comparable`; stdlib `min`/`max`/`clamp` unified into pure-Chezzi `std.cmp`; `list.sort()` widened.
- ✅ **Round 2 gaps #10–#15** — `sort_by`, `ord`/`chr`, int+float math, map `for`, nested/tuple
  match, bitwise ops. Plus: iterator protocol (struct `next()`), `Iterator[T]` parameterized bound
  with element recovery + lazy adapters, match guards + half-open range patterns.
- ✅ **Tuples + multiple return + destructuring (gap #8)** — `(e1, e2, …)`, tuple types, `a, b := f()`,
  `.0`/`.1` access; immutable, fixed-arity, GC-traced.
- ✅ **M6a/b/c** — core-type str/list methods; pipe `|>` (parse-time desugar); stdlib via the Level-2
  native FFI seam (`NativeFn` + `Host`): `std.math`/`std.io`/`std.os` native, `std.str` pure Chezzi.
- ✅ **`map[K, V]` dictionary (gap #5)** — literals, keyed read/insert/update, six methods, GC-traced.
- ✅ **Index & field assignment** — `xs[i] = v`, `p.x = v`, `+=`/`-=` mutate in place (both engines).
- ✅ **M5a/b/c** — bytecode compiler + stack VM; hand-built mark-sweep GC; cross-engine parity +
  perf (~6.5× arith / ~4.3× fib over the interp) + CLI default flip to the VM (`--interp` for the
  tree-walker). Documented divergence: VM pre-parses `{expr}` chunks (malformed interpolation in dead
  code is a load error). `std.os.getcwd` not yet injectable via `HostConfig`; `read_file` capped at 64 MiB.
- ✅ **M4.5 — modules / imports + resolver** — multi-file, `chezzi.toml` root, run-once dep order,
  cross-module home-globals, cycle detection. Type names are program-global (collision-detected).
- ✅ **M4 — type checker (local inference)** — bidirectional, no unification; return-type inference,
  `T?`/`T!` sugar, expression-valued `match`/`if`, Go-style error accumulation.
- ✅ **M3 — tree-walk interpreter** — full expr/stmt set, `?` operator, string interpolation,
  256 MB-stack thread + `MAX_CALL_DEPTH` guard.
- ✅ **M2.5 — canonical grammar + conformance** — `docs/grammar.bnf` executed via the `bnf` crate
  (dev-dep only), differential-tested vs the parser over a corpus. Run `cargo test conformance`.
- ✅ **M2 — parser → AST** — recursive descent + Pratt; spans retrofitted; depth-capped.
- ✅ **M1 — lexer** — full `examples/hello.chz` incl. Indent/Dedent; string escapes, numeric underscores.
  Open follow-ups (anytime): scientific notation `1e3`, single-quote strings, unicode `\u{…}` escapes.

---

## Roadmap (later)

- 🟦 **Concurrency C5 — Group B (real engine, VM)** — **B1 + B2 (cooperative fibers + blocking
  `recv`) done on the VM**. Remaining **B3/B4/B5 now planned as a phased epic** in
  [`docs/concurrency-b3.md`](docs/concurrency-b3.md) (B3.0…B3.6; B4 real `Shared` + B5 real `Executor`
  pool + A3b are folded into B3.4–B3.6 since shared-nothing threads make them the same machinery).
  **B3.0 (wire-format airlock) is done**; next code step: **B3.1** (move `Channel`/`Shared`/`Executor`
  cores out of the heap into `Arc<…Core>`, single-thread, parity-preserved).
  **interp B1/B2 is a deliberate non-goal** (see the DECISION box in Current focus — the interp stays
  the sequential-subset parity oracle; the VM is the sole concurrent engine). Group A is done: C1–C4,
  the `Executor` sequential subset, **A2 auto-drain**, the C5 checker refinements, **A3a** (pinned),
  and **A1** (`Channel.try_recv`, both engines). Only A3b is left (lands with Group B). See the A/B
  breakdown in `docs/concurrency.md` §9.
- VM/GC optimizations (superinstructions, inline caching, NaN-boxing) — written up in
  **[`docs/future.md`](docs/future.md)**.

### Ideas — record-only (not scheduled)

- **Native FFI / Rust-library bindings** — let Chezzi call into Rust libs; design sketch in
  `docs/spec.md` → *Standard library* → "Future idea — native FFI". Default build stays zero
  third-party crates; dynamic `cdylib` plugins deferred. Do not start without an explicit decision.

---

## Known friction / open (document-only)

Surfaced by coverage passes; no `src/` changes pending, recorded for when they bite:

- **Collection literals must be single-line** — a newline inside `[`/`{` ends the expression.
- **`match` limits** — no multiple `Some(...)` arms, no nested nullary-variant patterns (nest a
  second `match`).
- **Float division by zero is a runtime fault**, not an IEEE `Inf`/`NaN`.
- **`std.os.getcwd`** not yet injectable via `HostConfig` (parity holds); **`read_file`** capped at 64 MiB.

## Notes

- Recursive structs "just work" via the checker's two-pass name collection — trees and linked lists
  need only `Node?` child fields + a `match` per step, no special support.
