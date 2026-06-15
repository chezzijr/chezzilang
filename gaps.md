# Chezzi — Language Gaps

Known limitations discovered by writing real programs and probing the language against what an
everyday app needs. Each **open** entry lists what it **blocks** and a **fix sketch** where the path
is clear. Resolved gaps are kept as a one-line log (so we don't re-flag them) — full fix detail lives
in `PROGRESS.md` + the cited `examples/*.chz`.

> Method: small `.chz` snippets run through both engines (`chezzi run` / `--interp`). "Verified"
> means observed, not inferred from the cheat-sheet (`docs/syntax.md`).

Legend: 🔴 blocks real apps · 🟡 notable friction · 🟢 works (recorded so we don't re-flag it).

Last updated: 2026-06-15. Baseline: post-M18 (`defer` → block-scoped) + concurrency D6 complete; gaps pass II in progress.

> **Forward-looking brainstorm** (a non-Go concurrency model, VM/GC optimizations, far-out ideas)
> lives in **[`docs/future.md`](docs/future.md)** — speculative, NOT scheduled. Concrete near-term
> scripting features have been **promoted into the "Open gaps" section below**; future.md keeps the
> large/speculative tracks (BEAM concurrency, JIT, register VM).

---

## Open gaps

The language **core** is feature-complete: scalars, `list`/`map`/`set`/`tuple`, structs (generic),
sum types (`enum` with payloads, generic), `Result`/`Option` + `?`, generics + structural protocols
(`Comparable`/`Add`/`Sub`/`Mul`/`Hashable`/`Stringable`/`Error`/`Iterator[T]`/`Index[K,V]`/`IndexSet[K,V]`/`Slice[R]`), exhaustive `match`
(literals, wildcard, nested/tuple, guards, ranges), closures/HOF, struct methods, modules, GC, two
backends, string interpolation, pipe, panic recovery (`recover:`), default + named args. What remains
is **~70% stdlib/scripting breadth, ~30% type-system + runtime depth**, ordered below by leverage.

### 🔴 Scripting essentials (promoted from future.md) — ✅ all resolved

- ~~**Comprehensions** — `[x*2 for x in xs if x>0]`~~ — **resolved (`481514b`).** List, set, and dict
  forms (`{k: v for … if …}`), with optional guard, parse-time desugared to a loop + `push`/insert
  (no new opcode). Loop var scoped per `infer_comprehension` (deliberately not marked immutable —
  body is an expression, can't be assigned). Both engines.
- ~~**Sub-ranges — Rust-style `xs[1..3]`**~~ — **resolved (M15, see 🟢 + resolved log).** Shipped as
  `xs[start..end]` / `s[start..end]`, half-open + bounds-clamped, reusing the existing `..` range (no
  new lexer token). The earlier "hardcoded to list/str, not a protocol — not now" plan was **reversed**:
  rather than hardcode, the milestone landed the deliberate **pair** — `Index[K, V]` + `IndexSet[K, V]`
  (read / mutable indexing) and `Slice[R]` — as prebuilt structural protocols. Built-in `list`/`map`/`str`
  satisfy them intrinsically (like `Iterator[T]`); user structs satisfy them via `index`/`set_index`/`slice`,
  so `custom[k]`, `custom[k] = v`, and `custom[a..b]` now all work and a generic can be bounded by
  `Index[int, V]`. **Deferred extensions:** omitted bounds (`xs[..n]`/`xs[1..]`/`xs[..]` — needs
  optional-bound ranges), inclusive `..=`, and negative indexing on plain `[i]`.
- ~~**Generators (`yield`) + a formal `Iterator[T]` protocol**~~ — **resolved + descoped.** The
  `Iterator[T]` protocol shipped (M13, see 🟢): `[S: Iterator[T], T]` is a real parameterized bound.
  `yield`/generators are a **permanent non-goal** (see `spec.md` *Non-goals*) — they would have
  needed coroutine/continuation support in *both* engines, and are unnecessary: lazy
  `map`/`filter`/`take` are written as **adapter structs** over `Iterator[T]` (Rust's `std::iter`
  model — `examples/iter_adapters.chz`).
- ~~**`std.os.exit(code)` + real process exit codes**~~ — **resolved (`481514b`).** `os.exit(code)`
  halts immediately with the given process status, unwinding past `recover:` and skipping `defer`
  (hard exit). Exit code threads through both run drivers + the CLI. `examples/exit.chz`; both engines
  parity-tested (`vm::tests::exit_threads_code_through_both_engines`).

### 🟡 Scripting ergonomics (promoted from future.md)

- ~~**List concat + map merge**~~ — **resolved.** Method-based (no operator overload, decided):
  list `.concat(ys)→list` (new) / `.extend(ys)→nil` (in place); map `.merge(n)→map` (new, `n` wins on
  key clash) / `.update(n)→nil` (in place). New collections built fully before the single `alloc`
  (GC-safe); self-`extend`/`merge` snapshot the other side first. Reuses the existing list/map method
  dispatch — checker sigs (`list_method_sig`/`map_method_sig`), interp `builtins`/`eval_map_method`,
  VM `core_method`. `examples/concat_merge.chz` (golden + parity). (Spread/unpack `[*a, *b]`, `{**m}`,
  `f(*args)` stays **dropped** — variadics are a non-goal.)
- ~~**Hex / binary / octal literals** — `0xFF`, `0b1010`, `0o17`~~ — **resolved.** Lexer-only:
  `number()` detects a `0x`/`0b`/`0o` prefix (case-insensitive) and parses the body via
  `i64::from_str_radix`, `_` allowed between digits. Token stays `Int(i64)` so the value flows
  through unchanged on both engines. `examples/hex.chz` (golden + parity).
- ~~**Tuple-destructuring `for` (+ `enumerate` / `zip`)**~~ — **resolved.** `for a, b in pairs:` over a
  `list[(A, B)]` (N names over `list[tupleN]`); one name still binds the whole tuple. Checker
  `for_bindings` gained a `Ty::List(Ty::Tuple(ts))` arm (arity-checked) ahead of the multivar-error
  arm. Interp `iter_rows_from_value` expands a tuple element into a row when `vars > 1`. VM was
  type-erased, so `compile_for`'s multivar branch now splits at runtime on a new `Op::IsMap`: map →
  keys/values lockstep (unchanged); list-of-tuples → index the list + destructure each element via
  `GetField(j)` (the destructure-`:=` pattern, generalized). `enumerate` / `zip` shipped as pure-Chezzi
  `std/iter.chz` (no builtins, no checker arm) — empty-list element type flows from the `-> list[(...)]`
  annotation. Composes with comprehensions (`[a + b for a, b in iter.zip(xs, ys)]`).
  `examples/for_tuple.chz` (golden + parity).
- ~~**Optional chaining + null-coalescing** — `x?.field`, `a ?? default` on `Option`~~ — **resolved.**
  `x?.field` / `x?.method(args)` (None short-circuits, Some(v) applies + re-wraps → always `Option`,
  no auto-flatten) and right-assoc `a ?? b`. Lexer-adjacent `?.`/`??` tokens (`x? .f` stays
  try-then-field). Parser builds carrier nodes (`OptChain`/`NullCoalesce`); the **desugar pass** lowers
  them to a `match` on the `Option` (`match x: Some(__c): Some(__c.f); None: None`) — so the checker
  and **both engines need zero new code** (match + Some/None already work). `??` at bp 4 (looser than
  `and`, tighter than `or`). `examples/optchain.chz` (golden + parity).
- ~~**`defer` (cleanup on scope exit)**~~ — **resolved (M16, **block-scoped since M18**; see resolved
  log + `examples/defer.chz`).** Block/lexical-scoped: runs when the enclosing indented block exits
  (loop body, `if`/branch, `recover:`, `match` arm, fn body, module top level), LIFO, inner-first,
  on every exit path (fall-through, `break`/`continue`, return, `?`, panic). Args evaluated at the
  `defer` statement (Go). (Considered & rejected: Python-style `with` — needs a new protocol + block.)
- ~~**`defer:` block form (multi-statement deferred body)**~~ — **resolved (2026-06-11; see
  `examples/defer.chz` + `docs/syntax.md` §9 defer).** Mirrored `spawn`'s dual form 1:1 with **no new
  VM op**: AST `Defer(Expr)` → `Defer(DeferTarget::{Call,Block})`; `parse_defer` branches on `Colon`
  → `parse_block`; grammar `<deferStmt>` gained `| "DEFER" <block>` and moved into `<compoundStmt>`;
  checker splits the arm (Block = ordinary nested scope, **no** capture floor — same-thread);
  compiler's `compile_defer` Block arm builds a synthetic zero-arg proto then emits
  **`MakeClosure(pid, entries)` + `DeferCall(0)`** (reuses existing ops); interp added
  `Deferred::Block` snapshotting locals **shallow (`.clone()`, matching `MakeClosure`'s handle copy —
  NOT `deep_clone`/airlock)**, run via `run_block_task`. **Semantics:** body runs top-to-bottom at
  scope exit; LIFO as a unit relative to other `defer`s; free vars snapshot **by value at the `defer`
  point**; runs on all exit paths (return/`?`/break/continue/panic/`recover:`). Two review-found
  parity bugs were fixed before landing: (1) **reassigning** an enclosing local inside the block is
  now rejected at check time (`defer_floors` write-gate — a separate floor that does NOT engage the
  airlock read-sendability gate, so same-task non-sendable *reads* stay legal) instead of crashing
  the VM compiler (no `SetCaptured` op) and silently no-op'ing the interp; (2) a `?` short-circuit
  inside the block is **discarded** on both engines (the block has no error-return contract — VM runs
  it as a closure and drops the return; interp's `run_block_task` now absorbs the propagation like
  `call_closure`). Both engines byte-identical (golden + 4 VM parity tests + 5 checker tests).
  **Note:** the broader **captured-local write soundness gap for plain closures** (below) stays open
  — `defer:` closes only its own instance via the dedicated `defer_floors` gate.

### 🟡 Type-system + runtime depth (already-tracked open)

- **🟡 `break`/`continue` inside a `spawn:` / `defer:` block: checker accepts, engines diverge** —
  these blocks compile into a fresh child proto with an empty loop stack, so a `break`/`continue`
  lexically nested in an enclosing loop is rejected by the **VM** at runtime (`break outside loop`,
  from the compiler's child-proto isolation) but silently treated as a **block exit** by the interp,
  while `check` reports "ok". A clean `check` should guarantee the program runs, and the two engines
  must agree. Affects **both** block forms identically (pre-existing for `spawn:`, inherited by
  `defer:`). **Fix sketch:** treat a `spawn:`/`defer:` block as a control-flow boundary in the
  checker — save/zero `loop_depth` across `check_block` for these arms (`checker/mod.rs:1092`) so the
  `loop_depth == 0` guard at `StmtKind::Break` fires, rejecting `break`/`continue` at check time on
  both engines. One shared fix for both forms. **Confirmed on HEAD (2026-06-16):** `for i in 0..3:`
  wrapping a `defer:` block with `break` (and a `spawn:` block with `continue`) — `check` → `ok`,
  VM `run` → `break`/`continue outside loop`, `--interp` → prints `0 1 2`. Three-way divergence
  reproduced; repros at `/tmp/brk_defer.chz` / `/tmp/brk_spawn.chz`.
- **⚪ Checker `=` to a by-value captured local — UNREACHABLE on HEAD (verified 2026-06-16; latent, not a live soundness gap)** — `infer_closure`
  pushes **no** capture floor (`checker/mod.rs:2841`), so an inner fn/closure that writes an
  **enclosing fn's local** (`x = 6`) *would* type-check (no capture floor), and the compiler can't
  resolve the local, so it *would* misroute the store to `SetGlobalSlot` (`emit_store`, `compiler/mod.rs:932` — there is no
  `SetCaptured` arm; loads have `GetCaptured` at `:943`, stores don't) → it would write a phantom
  global and leave the outer local **unchanged** (silent no-op). **Verdict (2026-06-16): UNREACHABLE
  — no surface syntax drives a closure/inner-fn capture-write that the checker accepts.** Closures
  parse a single expression (`parser/mod.rs:1873`) and assignment is a *statement*, not an expression,
  so `inner := fn(): x = 6` is a parse error ("expected end of line, found '='"); a statement-bearing
  nested named `fn` *can* hold `x = 6`, but the checker rejects the nested fn name outright (`unknown
  name 'inner'`, even for a benign body) so it never type-checks; and the `spawn:`/`defer:` block forms
  are already guarded (`capture_floors`) and correctly error. The `emit_store` misroute is therefore
  **latent** — it would surface only if block-bodied closures (or a checker that accepts nested-fn
  names) ever land; re-open then. `docs/syntax.md:828` already promises "captures are
  copies, **read-only** inside a task (reassign = error)" — but that rule is enforced **only** across
  `spawn`/`submit` (the only `capture_floors` push sites, `checker/mod.rs:1306`/`:3355`), not plain
  closures. **Blocks:** nothing today (unreachable); the latent risk is the docs' own read-only
  guarantee, should a future syntax (block-bodied closures / nested-fn names) expose it. **Fix sketch (pre-emptive):** generalize
  the spawn read-only check to all closure bodies — push a capture floor at the closure's scope depth
  in `infer_closure` — turning the silent misroute into the documented `cannot reassign captured
  binding` error, with a "use `:=` to shadow, or `Ref[T]` (in-task) / `Shared[T]` (cross-task) to
  mutate" hint. Module-global mutation stays (same global slot, no misroute). Cross-language note:
  Python needs `nonlocal`/`global` to rebind outer; JS/Go/Lua capture by ref; Chezzi is by-value
  snapshot (closer to C++ `[=]` / Java effectively-final). **Parked companion idea (not scheduled):**
  a deref-sugar to make the `Ref[T]` escape hatch ergonomic — `r^ += 1` / `print(r^)` desugaring to
  `set`/`get` field ops (no new VM op; `Ref` is already an `Rc<RefCell>` struct), so the read-only
  rule doesn't feel like a punishment.

- ~~**Non-constant default expressions**~~ — **resolved (relaxed).** A default may now be any
  expression that does **not** reference another parameter/field — `compute()`, `1 + 2`,
  `GLOBAL * 2` all work (params + struct fields). The parser dropped its const-literal restriction;
  the desugar pass (`validate_defaults`) rejects a default that references another param/field
  (defaults are cloned into the **caller's** scope at the omitting call site, where params/fields are
  not bound). Function-call defaults run once per omitting call. `examples/default_expr.chz` (golden +
  parity). **Still out:** param-referencing defaults (`y: int = x + 1`) — would need call-time eval
  in the callee frame.
- ~~**Calling a function-typed field**~~ — **resolved.** `recv.f(x)` where field `f: fn(T)->U` now
  resolves to field-access-then-call (on `self` and on an external receiver). Three layers:
  desugar `normalize_call` is **field-aware** (a program-wide set of `fn`-typed field names skips
  method-default normalization → no same-named method's default is injected into a fn-field call);
  the checker's `infer_method_call` struct arm falls back to a `Ty::Func` field (type-arg
  substituted) when no method matches; both engines (`call_struct_method` / VM struct dispatch) fall
  back to calling the field value. `examples/fn_field.chz` (golden + parity); `examples/iter_adapters.chz`
  now calls `self.f(x)` directly. **Narrow limitation (accepted):** if a program uses a name as both
  a `fn`-typed field *and* a method on different structs, that method loses desugar-time argument
  normalization — an omitted default isn't filled (→ arity error) and a named-arg call is rejected;
  call it positionally with all args. The receiver type is unknown in the pre-type desugar pass, so
  the field-vs-method ambiguity can't be resolved there.
- ~~**`sort_by_key`**~~ — **resolved.** Native list method `xs.sort_by_key(f: fn(T) -> K)` — sorts in
  place by a derived key, sugar over `sort_by` (#11). `K` must be Comparable (int/float/str or a
  struct with `compare`); keys are computed once per element, then compared by natural order (scalar
  ordering / struct `compare`). Stable. Mirrors `sort_by`'s GC-rooted re-entrant merge sort in both
  engines (VM roots a parallel keys list). `examples/sort_by_key.chz` (golden + parity).
- ~~**`Ref[T]` — a lightweight mutable box**~~ — **resolved.** Pure-Chezzi `std/ref.chz`:
  `struct Ref[T]: value: T` + `get`/`set`/`update(f)`. Capture-by-value snapshots a bare `int`, but a
  `Ref[T]` is a shared struct (`Rc<RefCell>`), so a closure that closes over it and mutates it through
  a method persists the change. **No engine change** (generic struct + self-mutation + fn-param call
  already work). Types are program-global, so `import std.ref` makes `Ref` usable by its bare name.
  `examples/ref.chz` (golden + parity). The cross-task counterpart **`Shared[T]`** (owner-task +
  channel, same API) lives in `docs/future.md` §2 — not built here.
- ~~**Runtime stack traces**~~ — **resolved.** An uncaught runtime fault now prints the error line
  plus the call chain (innermost first) with each call's line:
  `runtime error (line 12, col 12): division by zero` / `  at divide (called at line 15, col 12)` /
  `  at compute (…)` / `  at main (…)`. Both engines produce **identical** traces (each frame carries
  the call-site span + function name). VM captures from `self.frames` at the uncaught fault before the
  unwind (`fault_trace`, reset by `recover:`); interp keeps a `call_stack` popped only on success
  (`recover:` truncates it). `RunError` wraps `RuntimeError` + trace at the run boundary — engine
  `RuntimeError` and the parity-tested `Display` are unchanged. `examples/stack_trace.chz`. (Cost: a
  fn-name clone per interp call — acceptable for a scripting language; optimizable later.)
- **Integers** — ~~overflow policy~~ **resolved.** Overflow is now a single, documented, fully-tested
  policy: **every `i64` overflow is a recoverable panic** (`RuntimeError "integer overflow in <op>"`,
  catchable by `recover:`) — never a silent wrap, never a host crash. Was 95% there (`checked_*` on
  `+ - * / % neg`, shifts bounds-checked, literal overflow → `LexError`); the one leak was
  `std.math.abs(i64::MIN)` using raw `i64::abs()` (panic in debug / wrap in release) — fixed to
  `checked_abs` → recoverable fault. `examples/overflow.chz` (golden + parity, both engines).
  **Still `i64`-only by design:** no `byte`/`u8` scalar (Python model — Python has no byte scalar
  either; binary data is a `bytes` *sequence* of ints 0..255, and Chezzi already mirrors this for
  chars: "no `char` type", 1-char `str`). Binary/buffer work is deferred to a future **`bytes`
  sequence type** (stdlib-breadth track, see "binary/crypto" above), not a new scalar. **bignum**
  (arbitrary precision) stays a non-goal.
- **✅ Reassigning a loop variable** — `for i in 0..3: i = i + 100` used to diverge (VM mutated the
  live counter slot → one iteration; interp advanced an internal counter → ran all three). **Fixed:**
  the checker now rejects assignment (`=`/`+=`/`-=`) to any `for`-loop variable — they're fresh
  per-iteration bindings (Python/Rust semantics), so the divergent program never reaches either
  engine. See `src/checker/mod.rs` (`is_loop_var`/`mark_loop_var`) + checker tests
  `for_*_reassign_rejected`.

### 🟡 Concurrency (engine — deeper tracking in `docs/concurrency.md` §11 + `docs/concurrency-tier-d.md`)

The `--parallel` engine is a true M:N scheduler through **D6** (fibers, work-stealing, dirty pool,
netpoller + `std.net`). The items here are *correctness/semantics* gaps found by probing, not missing
breadth. The big deferred *features* `Channel.close()` and the per-socket read/accept/write **timeout**
(D6c) have landed; **per-connection `spawn`** remains deferred in the tier-d doc. (D5 owe #3
`recv`-in-native-callback: Path A landed for `iter.*`; **Path C
thread-demotion landed (attempt)** for the native islands — see the residuals entry below.)

- ~~**🟡 Pending `spawn` tasks are silently dropped on an early escape from `parallel:`**~~ —
  **resolved 2026-06-11 → cancel-and-report (both engines).** A `parallel:` body escaping via
  `?`/`return`/`break`/`continue` before the join now **cancels** its unstarted `spawn` tasks (the
  same end-state a started sibling reaches under B3.4) and emits one stdout report line
  (`runtime::pending_cancel_report`, byte-identical across interp / VM-cooperative / VM-`--parallel`);
  the escape propagates unchanged, nursery depth returns to 0. This also fixed the prior engine
  divergence (interp ran unstarted tasks on `return`/`break`, dropped on `?`; VM dropped on all three —
  now all three cancel-and-report uniformly). VM routes `drain_escaped_nursery` through four reclaim
  sites (`do_return`, recover-catch fault, net-new `Op::ReclaimNursery` for break/continue, `do_try`
  recover-scoped `?`); a review-caught Critical on the `do_try` path (report ordered before a
  parallel-**body** `defer`) was fixed with a per-nursery `nursery_defer_floors` stack so body defers
  drain before the report, recover-block defers after — interp order. (Surface note unchanged:
  `spawn f()?` is a *parse* error; `?` inside a spawned body/`spawn:` block faults the task.) Decided
  against run-pending-to-completion (Policy 1): deadlock-prone — a pending `recv` awaiting a `send`
  from the escaped parent — and would need a scheduler mid-unwind with no serial-oracle analogue.

- ~~**🟡 VM leaks the nursery on a `?`/return escape from `parallel:` not caught by `recover:`**~~ —
  **resolved (see resolved log).** `do_return` now `truncate`s `self.nurseries` to the frame's
  entry depth, mirroring `Handler::nursery_len` + the interp's unconditional `exec_parallel` pop.

- **🟡 Path C (recv-in-native-callback thread-demotion) residuals** — **D5 owe #3 Path C landed as an
  attempt (2026-06-11, branch `d5-owe3-path-c`).** A blocking `recv` inside a native callback
  (`xs.map`/sort comparator/`Shared.update`) now **demotes the worker thread** (blocks in place +
  spins a replacement) and resumes on a sibling `send`, instead of faulting `deadlock` (M:N engine
  only; the cooperative engine still faults). #1, #3-sleep resolved 2026-06-11; **#3-socket resolved
  2026-06-12**; only #2 (WON'T FIX by design) remains — **full worklist (corrected examples + fix
  sketches + cross-language refs) in `docs/concurrency-tier-d.md` → "Path C residuals — worklist".**
  - ~~**(#1) narrow deadlock false-positive**~~ — **resolved (2026-06-11).** `SchedCore` now registers
    each demoted fiber's `ChannelCore` (refcounted); `is_deadlocked` peeks the registered queues and
    vetoes the fire if any holds a value (that fiber will pop + progress), and the demote loop's
    `pop + blocked_native-- + un-register` is atomic under the core lock (A-then-q) so the checker never
    sees an emptied-but-still-counted demoted fiber. No more spuriously-killed parked sibling. White-box
    (`deadlock_predicate_vetoed_by_queued_value_on_demoted_channel`, `..._refcounted_for_two_fibers_...`)
    + 200× black-box stress regression tests.
  - ~~**(#3) socket/sleep op inside a callback**~~ — **fully resolved.** ~~`sleep_ms` half~~ (2026-06-11):
    a `sleep_ms(ms>0)` reached inside a native callback **demotes** (spawns a replacement + sleeps in
    place + resumes, accounted `inflight` so it vetoes deadlock) instead of running inline + pinning the
    worker. ~~Socket half~~ (2026-06-12): a `read`/`write`/`accept` that `WouldBlock`s inside a callback
    now **demotes** too — `demote_block_socket` spins a replacement and **kernel-blocks on the fd**
    (`libc::poll`, `DEMOTE_POLL_BACKOFF` timeout) re-running the non-blocking op on readiness, accounted
    `inflight` (netpoller-park parity: vetoes deadlock, a lone in-callback `accept` with no client never
    self-terminates). Was surfacing a misleading `"…require the --parallel engine"` error. *Perf/liveness,
    not wrong.* (`connect`-in-callback unchanged — handshake state in `pending_connect`, even rarer.)
  - **(#2, WON'T FIX by design) `Shared.update` same-box hold-and-wait** — a `recv` blocking inside
    `update(f)` holds `update_lock` while parked → a sender needing the *same* box deadlocks silently.
    A **universal** hold-and-wait deadlock (Go detects only the global case, not partial; Rust lints it
    via `await_holding_lock`; BEAM has no shared locks). Dev-authored. Rule: *don't block on a value that
    needs the same `Shared` box.* Future: a lint/warning when the tooling track lands.
  - **(cost, by design)** one raw OS thread per fiber *actually* blocked in a callback (Go's `handoffp`
    cost), faulted cleanly if the OS refuses the thread.

- **✅ SHIPPED — first-class task cancellation / timeout API (`std.cancel`).** A user-level
  cooperative cancellation **`Token`** (`std/cancel.chz`, Go-`context`-inspired): `cancel.manual()` /
  `cancel.timeout(ms)`, methods `cancelled() -> bool`,
  `reason() -> str?` (`"cancelled"`/`"timeout"`), `done() -> Channel[bool]` (a `wait:` arm), `cancel()`
  (anytime, any task), `deadline_at()`. Tokens are **flat** in v1 (no parent/child derivation — tree
  propagation is a documented follow-up). Built over `Shared[bool]` + `monotonic()` (deadline checked
  **at poll time**, so the timeout case is deterministic across engines — no background canceller) + the
  one new native primitive **`Channel.trip()`** (a permanent level-trigger latch, the manual-cancel
  fan-out a move-on-send `Channel` can't give). **Deliberately decoupled** from the internal nursery
  `cancel: Arc<AtomicBool>` (which is still tripped only by a sibling fault `src/vm/mod.rs:6391` or
  `std.os.exit` `:10642`) — a user `cancelled()`-driven `return` runs `defer`/`recover:` normally,
  unlike the internal scope-cancel unwind that bypasses them (`:2856-2858`). The old naive `wait:`/
  `timer` "timeout" remains wrong (it returns `Err("timeout")` logically but runs the full work — a
  task can't outlive its nursery); `std.cancel` is the supported answer for timeouts + manual cancel.
  **Test-authoring rule (parity):** a cancellation example is golden only if it asserts
  *that* cancellation happened (which outcome / which `wait:` arm) at a fixed point — never iteration
  count or *when* a CPU loop was interrupted. Manual cancel of a running CPU sibling diverges by engine
  (see below), so `examples/cancel_cpu.chz` carries no `.expected` (joins `examples/parallel_cancel.chz`)
  and is covered by a Rust `#[test]` instead. See `docs/concurrency.md` §6e / §6c'.

- **🟡 The cooperative (default) + `--interp` engines cannot preempt CPU-bound tasks** — cooperative
  engines switch fibers only at yield points (channel ops, blocking `recv`, the back-edge *when it
  returns to the scheduler*). A pure-CPU loop with no channel op never yields → it **monopolizes the
  single thread**, so sibling tasks (a canceller/timeout) never run and the cancel flag is never polled
  mid-loop. **Consequence:** the *same source* diverges by engine — the cancel-and-continue workaround
  above, with a 2e9-iteration CPU worker: `--parallel` aborts it mid-flight (`s == 0`, ~0.5s);
  cooperative + `--interp` run it to completion (`s == 2000000000`, **~69s**). IO/channel-bound tasks
  *can* be cancelled cooperatively (they hit yield points); pure-CPU tasks cannot. This is why
  cancellation examples carry **no golden `.expected`** — their output diverges by engine, so they'd
  fail two-engine parity by construction (`examples/parallel_cancel.chz` has none). **Implication:** any
  per-task timeout would only catch a runaway task under `--parallel`, never a CPU-spinning one on the
  default engine. **Fix sketch:** reduction-counting preemption already exists on the M:N engine (D3,
  `:2871`); the cooperative engine would need a back-edge yield budget for CPU loops to be interruptible
  — a behavior change weighed against the frozen cooperative oracle's determinism.

### 🟡 Stdlib breadth (low priority — language is feature-complete; this is library fill)

> **Not necessary for now.** The current stdlib (`std.fs`/`io`/`os`/`process`/`time`/`request`/
> `regex`/`json`/`math`/`cmp`/`str`/`iter`) covers **read-and-transform** scripting well — text
> processing, JSON/HTTP/API work, file *reading*, shelling out, regex, DSA. The gaps below block
> **write-heavy automation, randomness, binary/crypto, and CLI tooling**. Ranked by leverage.
>
> Split matters (ties to the bootstrap/tooling track): the **native** items touch syscalls/entropy
> and must be Rust; the **pure-Chezzi** items are writable as `std/*.chz` modules today (the language
> is feature-complete) and make good dogfood / bootstrap-feasibility probes.

**Must be native (Rust):**
- **`std.rand`** — no RNG anywhere today (`std.math` has none). Highest leverage: unblocks shuffling,
  sampling, test data, tokens, sims, games. Small to add (OS entropy → seedable PRNG).
- **fs mutations** — `std.fs` is read-mostly (`exists`/`is_dir`/`is_file`/`list_dir`/`size`/`glob` +
  `io.read_file`/`write_file`). Missing `mkdir`/`remove`/`rename`/`copy`/`append` → can't manage files/dirs.
- **Encoding / crypto** — no base64, hex, sha/md5, uuid, url-encode. Common for API + hashing scripts.
- **`std.math` fill** — no trig (`sin`/`cos`/`tan`), no `log`/`ln`/`exp`. Blocks geometry/scientific.
- **`std.process` polish** — only `sh -c` (shell-injection-prone); captures stdout **or** stderr, not
  both + exit code. Want a structured result + an args-array form (no shell).
- **`std.request` polish** — get/post only (no put/delete/patch), thin header/timeout/query control.

**Writable as pure-Chezzi `std/*.chz` now (no native needed):**
- **path ops** (`join`/`basename`/`dirname`/`ext`/`normalize` — scripts hardcode `/` today),
  **`argparse`** (raw `os.args` only), **CSV** (json exists, csv doesn't), **duration / date
  decomposition** (timestamps only — no year/month/day/parse/duration math), higher-level
  **data structures** (heap / priority-queue, deque, counter, ordered map) — all expressible with the
  current generics + protocols. (**`Ref[T]`** shipped — see resolved log.)

**Language-level (separate, see *Type-system + runtime depth*):** `i64`-only + no `byte` type blocks
clean binary/buffer work — relevant to encoding above and to any future self-host.

### Tier 4 — ecosystem (toolchain, not the language)
REPL (huge for scripting iteration), formatter, `assert` + built-in test runner, LSP, package
manager / registry (spec defers this), debugger, doc comments + docgen.

---

## 🟢 Verified working (so we don't re-flag)

- **Struct equality** `P(1,2) == P(1,2)` → structural compare.
- **String indexing** `s[i]` → 1-char `str`; `s.len/upper/lower/trim/split/join/contains/starts_with`;
  `s.chars()` + strings iterable (`for c in s`).
- **List-of-structs**, field access `ps[1].y`; **nested-list read** `g[i][j]`; **by-reference
  sharing** — a list passed to a fn and `.push`ed is mutated for the caller.
- **`if` / `match` as expressions**, incl. inside interpolation `"{if a>b: a else: b}"`.
- **`Result` / `Option` + `?`**, exhaustive-match checking, deep recursion, **integer overflow → a
  recoverable panic** (every op + negation + `MIN / -1` + `math.abs(MIN)`; `recover:`-catchable, never
  wraps), int division truncation, `%` on negatives. `examples/overflow.chz`.
- **`std.math` / `std.io` / `std.os` / `std.str` / `std.cmp` / `std.json` / `std.time` / `std.fs` /
  `std.process` / `std.regex` / `std.request`** on both engines.
- **Recursive / self-referential structs** (BST, linked list) build, walk, GC fine.
- **Mutable `self` across method calls** — `self.pos += 1` persists for the caller (recursive-descent
  parser cursor relies on it).
- **Nested-list DP** — `list[list[int]]` with two-level `dp[i][w] = …` index assignment.
- **Empty map literal infers `K,V` from later use** — `m := {}` then `m["a"] = 1` type-checks.
- **User-struct iterator protocol** — a struct with `next(self) -> Option[T]` is iterable in `for`
  (lazy per-step; infinite + early `break` terminates). Both engines.
- **`Iterator[T]` parameterized bound** (M13) — `[S: Iterator[T], T]` accepts any iterable (built-in
  `list`/`set`/`str`/`map` intrinsically, or a struct via `next`) and recovers element type `T` into
  loop vars + return types. The first protocol that takes type arguments — now generalized to
  **user-defined parameterized protocols** (M14, `protocol Container[T]`). Lazy adapter structs
  (Take/Mapped over an infinite source) compose without `yield`. Both engines parity-tested.
  `examples/iterator_bound.chz`, `examples/iter_adapters.chz`.

---

## Resolved log (one line each — full detail in `PROGRESS.md` + examples)

**Round 1 (#1–#9) ✅** · both engines lockstep, parity + conformance green:
1. **Index assignment** `xs[i] = v` (+ `+=`/`-=`) — `Op::SetIndex`/`Dup2`. `examples/mutate.chz`.
2. **Mutable struct fields** `p.x = v` (+ compound) — `Op::SetField`/`Dup`.
3. **HOF params** `f: fn(int) -> int` — `Type::Func` + `resolve_type` lowering. `examples/hof.chz`.
4. **List methods** `pop`/`reverse`/`contains`/`index_of`/`sum`/`sort` + `map`/`filter`/`fold`
   (re-entrant, GC-rooted). `examples/list_methods.chz`, `list_hof.chz`.
5. **Map type** `{"a":1}`, `m[k]`/`m[k]=v`, `get`/`has`/`keys`/`values`/`remove`/`len`. `examples/map.chz`.
6. **Literal + wildcard `match`** (`0:` / `_:`) — `Pattern::Literal`/`Wildcard`, no new opcode.
7. **`break` / `continue`** — `LoopCtx`; for-`continue` lands on the increment. `examples/loops.chz`.
8. **Tuples + multi-return + destructuring** `(a,b)`, `(int,int)`, `a,b := …`, `.0`. `examples/pair.chz`.
9. **Strict compound assignment** — `+=`/`-=` reject `int <op> float` into an `int` slot.

**Round 2 (#10–#15) ✅** · real DSA/apps probe:
10. **`ord` / `chr` builtins** — char→int / int→char. `examples/cipher.chz`.
11. **`sort_by(fn(T,T)->int)`** — stable merge sort over a re-entrant comparator, GC-rooted.
    `examples/sort_by.chz`.
12. **Int `abs`/`min`/`max`** — later unified into generic `std.cmp` (M7-G3). `examples/knapsack.chz`.
13. **Bitwise ops** `& | ^ << >>` (int-only) — lexer→checker→both engines + grammar. `examples/bits.chz`.
14. **Map iteration** `for k in m` / `for k, v in m`. `examples/word_freq.chz`.
15. **Nested / tuple match patterns** — recursive `Pattern` + `MatchKind::Tuple`. `examples/match_nested.chz`.

**M7 — generics ✅** · generic functions + structs + structural protocols (`Comparable`); explicit
call-site type args `max[int](…)`; generic enums; multi-bound `T: A + B`. `examples/generics.chz`,
`generic_structs.chz`, `generic_enum.chz`.

**M8 — Tier-1 stdlib ✅** · `std.json` (dynamic `Json` enum + typed `decode[T]`), `std.time`,
`std.fs`, `std.process`; `s.chars()` + string iteration; `set` type. `examples/json_*.chz`, `set.chz`.

**M9 — Tier-2 stdlib ✅** · `std.regex` (regex crate), `std.request` (blocking HTTP via ureq+rustls);
seam grew `NativeRet::Struct`/`Map`. `examples/regex_demo.chz`, `request_demo.chz`.

**M10 — type-system depth ✅** · `Hashable` (real hash-table map/set, struct keys), `Stringable`
(custom `str()`/`print`/interp), `Add`/`Sub`/`Mul` operator protocols, multi-bound, type aliases
(`type UserId = int`). `examples/hashmap_keys.chz`, `stringable.chz`, `operators.chz`, `type_alias.chz`.

**M11 — Tier-3 robustness ✅** · panic recovery (`recover:` → `Result[T, Error]`, catches index-OOB
/ div-zero / overflow / missing-key); Go-style errors (`T!` = `Result[T, Error]`, `Error` protocol);
structural iterator protocol; match guards (`pat if cond:`) + range patterns (`1..10:`); default +
named args (functions + struct constructors, desugar pass). `examples/match_guard.chz`,
`match_range.chz`, `default_args.chz`, `named_struct.chz`.

**M14 — generics depth ✅** · two gaps closed (TDD, both engines parity-tested):
- **Method-level type parameters** — a method may introduce its own fresh `[U]` beyond the struct's
  `[T]` (`fn map_to[U](self, f: fn(T) -> U) -> U`); `U` is inferred from the call args, bounds
  enforced, recovered through `Iterator[T]` — the free generic-fn path generalized to method calls
  (`infer_generic_method`). Shadowing the struct's own param is rejected. `examples/method_type_params.chz`.
- **User-defined parameterized protocols** (concrete-arg bounds) — `protocol Container[T]:` plus
  bounds like `[X: Container[int]]`; conformance is structural with `T` substituted, the method's
  return flows into the caller. Generalizes the special-cased `Iterator[T]` (which still recovers its
  arg; user protocols take theirs explicitly). Usable as a bound only, not an existential value type.
  `examples/param_protocol.chz`.
- **Defaults / named args on methods** — now consistent with free fns + struct ctors. Handled in the
  pre-type **desugar pass** (`src/desugar`): a program-wide method registry resolves a call by name
  (the receiver type isn't known pre-check), fills omitted defaults + reorders named args into a
  positional list — so the checker and both engines stay untouched. Same-named methods on different
  structs with different params → a named call is ambiguous (rejected); built-in method names are
  skipped. `examples/method_default_args.chz`.

**M15 — slicing + the indexing protocols ✅** · (TDD, both engines parity-tested) — `xs[1..3]` /
`s[0..2]`, half-open + bounds-clamped, reusing the existing `..` range. Landed the deliberate
`Index[K, V]` + `IndexSet[K, V]` + `Slice[R]` prebuilt protocol **pair** (reversing the earlier
"hardcode, not now" plan): built-in `list`/`map`/`str` conform intrinsically (mirroring `Iterator[T]`),
user structs structurally via `index`/`set_index`/`slice`. So `custom[k]`, `custom[k] = v`,
`custom[a..b]` work, and a generic can be bounded by `Index[int, V]` (`K`/`V`/`R` recovered at the call
site). `examples/slicing.chz`.

**M18 — `defer` → block-scoped ✅** · (TDD, both engines parity-tested; reviewed) — supersedes M17's
frame-scoping. A `defer` runs when its **enclosing lexical block** exits (loop body, `if`/branch,
`recover:`, statement-form `match` arm, fn body, module top level), LIFO within a block, inner-first
across nesting, on every path (fall-through, `break`/`continue`, return, `?`, panic). Return/`?`/panic
keep the whole-frame LIFO drain (inner-first falls out free); only fall-through/break/continue/recover
got new drains. VM: `Op::EnterDeferScope`/`LeaveDeferScope` + `CallFrame.defer_markers` (emitted only
for defer-holding blocks → defer-free code byte-identical); `recover:` drains via `Handler.defer_len`
on Ok (`DrainHandlerDefers`) / fault / `?` paths, with `Handler.markers_len` truncating leaked
nested-scope markers (review fix). Interp: per-block `exec_block` finally-drain + `eval_recover_body`
finally. Top-level defer now legal (`in_fn` ban dropped). `examples/defer.chz` (block-scope section).

**M16 — `defer` (frame-scoped) ✅** · (TDD, both engines parity-tested) — Go-style `defer <call>`:
frame-scoped (function/method/closure), LIFO, drained on every exit path (normal return, `?`
short-circuit, panic). Receiver + args evaluated at the `defer` statement; the call runs at exit.
Per-frame deferred stack drained via the existing re-entrant invoke (`call_value` / `invoke_value` +
method dispatch); interp `finish_frame` teardown, VM `do_return` + handler-stack unwind
(`unwind_deferred`), GC-rooted on the frame. Targets a method or first-class-value call
(built-ins/ctors must be wrapped — checker-enforced); composes with `recover:`; `std.os.exit` skips
defers (matches Go). Interp thread stack 256→384 MB to keep the `MAX_CALL_DEPTH` guard ahead of the
slightly larger frames. `examples/defer.chz`.

**Tech debt cleared ✅** · parser `MAX_DEPTH` 128→64 (off the test-stack edge); duplicate type param
`[T, T]` rejected; nested-`set` equality parity; explicit call-site type args; `?`-in-closure checked
against the closure's own return type.

**Integer overflow policy ✅** · (TDD, both engines parity-tested) — formalized "every `i64` overflow
is a recoverable panic, never a wrap or host crash." Fixed the one leak: `std.math.abs(i64::MIN)` used
raw `i64::abs()` (panic in debug / wrap in release) → now `checked_abs` → `"integer overflow in abs"`,
catchable by `recover:` (`src/native/math.rs`). All other paths (`checked_*` arith, negation,
`MIN / -1`, bounds-checked shifts, lexer literal overflow) were already correct — now guarded by
regression tests. `i64`-only kept by design: no `byte`/`u8` scalar (Python model — binary work →
future `bytes` sequence type); bignum a non-goal. `examples/overflow.chz` (golden + parity).

**VM nursery-leak on `?`/return escape ✅** · (TDD, white-box) — a `?`/`return` escaping a `parallel:`
body skips its `JoinNursery`, so the VM's `do_return` left the nursery (+ its GC-rooted pending-task
args) on `self.nurseries` until program exit, while the interp's `exec_parallel` always pops — a
VM/interp divergence (internal memory only; output identical). Fixed by recording `nursery_len` on the
`CallFrame` at entry (mirroring `Handler::nursery_len`) and `self.nurseries.truncate(frame.nursery_len)`
in `do_return` — a no-op on the normal path (`JoinNursery` already popped), reclaiming only the
escaped nursery. Drop semantics unchanged (gap #1 still open). Black-box parity can't see the leak, so
the regression tests assert the residual nursery depth via a new `run_capture_nursery_len` helper
(`parallel_return_escape_leaves_clean_nursery_stack` + `..._try_escape_...` + the recover-caught
boundary `..._try_caught_by_recover_...`). `src/vm/mod.rs`. **Residual (tracked, not a leak-to-exit):**
a `break`/`continue` out of a `parallel:` inside a loop stays *in-frame* (a plain jump, no
`do_return`), so its nursery is reclaimed only when the enclosing **function** returns — bounded to
frame lifetime, output always correct, but not *block*-scoped like the interp's `exec_parallel` pop
(`src/interp/mod.rs:902`). Closing that last divergence would need loop-exit-jump codegen to emit a
nursery-drain when a `break`/`continue` crosses a nursery boundary (mirroring `emit_loop_body_drain`
for defer scopes); deferred with the gap #1 drop-vs-drain design call above.
